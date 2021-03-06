/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use cookie_rs::Cookie as CookiePair;
use devtools_traits::HttpRequest as DevtoolsHttpRequest;
use devtools_traits::HttpResponse as DevtoolsHttpResponse;
use devtools_traits::{ChromeToDevtoolsControlMsg, DevtoolsControlMsg, NetworkEvent};
use flate2::Compression;
use flate2::write::{GzEncoder, DeflateEncoder};
use hyper::header::{Accept, AcceptEncoding, ContentEncoding, ContentLength, Cookie as CookieHeader};
use hyper::header::{Authorization, Basic};
use hyper::header::{Encoding, Headers, Host, Location, Quality, QualityItem, qitem, SetCookie};
use hyper::header::{StrictTransportSecurity, UserAgent};
use hyper::http::RawStatus;
use hyper::method::Method;
use hyper::mime::{Mime, SubLevel, TopLevel};
use hyper::status::StatusCode;
use msg::constellation_msg::PipelineId;
use net::cookie::Cookie;
use net::cookie_storage::CookieStorage;
use net::hsts::HstsEntry;
use net::http_loader::LoadErrorType;
use net::http_loader::{load, LoadError, HttpRequestFactory, HttpRequest, HttpResponse, UIProvider, HttpState};
use net::resource_thread::{AuthCacheEntry, CancellationListener};
use net_traits::{LoadData, CookieSource, LoadContext, IncludeSubdomains};
use std::borrow::Cow;
use std::io::{self, Write, Read, Cursor};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, mpsc, RwLock};
use url::Url;
use util::prefs;

const DEFAULT_USER_AGENT: &'static str = "Test-agent";

fn respond_with(body: Vec<u8>) -> MockResponse {
    let headers = Headers::new();
    respond_with_headers(body, headers)
}

fn respond_with_headers(body: Vec<u8>, mut headers: Headers) -> MockResponse {
    headers.set(ContentLength(body.len() as u64));

    MockResponse::new(
        headers,
        StatusCode::Ok,
        RawStatus(200, Cow::Borrowed("Ok")),
        body
    )
}

fn read_response(reader: &mut Read) -> String {
    let mut buf = vec![0; 1024];
    match reader.read(&mut buf) {
        Ok(len) if len > 0 => {
            unsafe { buf.set_len(len); }
            String::from_utf8(buf).unwrap()
        },
        Ok(_) => "".to_owned(),
        Err(e) => panic!("problem reading response {}", e)
    }
}

struct MockResponse {
    h: Headers,
    sc: StatusCode,
    sr: RawStatus,
    msg: Cursor<Vec<u8>>
}

impl MockResponse {
    fn new(h: Headers, sc: StatusCode, sr: RawStatus, msg: Vec<u8>) -> MockResponse {
        MockResponse { h: h, sc: sc, sr: sr, msg: Cursor::new(msg) }
    }
}

impl Read for MockResponse {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.msg.read(buf)
    }
}

impl HttpResponse for MockResponse {
    fn headers(&self) -> &Headers { &self.h }
    fn status(&self) -> StatusCode { self.sc }
    fn status_raw(&self) -> &RawStatus { &self.sr }
}

fn redirect_to(host: String) -> MockResponse {
    let mut headers = Headers::new();
    headers.set(Location(host.to_owned()));

    MockResponse::new(
        headers,
        StatusCode::MovedPermanently,
        RawStatus(301, Cow::Borrowed("Moved Permanently")),
        b"".to_vec()
    )
}

struct TestProvider {
    username: String,
    password: String,
}

impl TestProvider {
    fn new() -> TestProvider {
        TestProvider { username: "default".to_owned(), password: "default".to_owned() }
    }
}
impl UIProvider for TestProvider {
    fn input_username_and_password(&self) -> (Option<String>, Option<String>) {
        (Some(self.username.to_owned()),
        Some(self.password.to_owned()))
    }
}

fn basic_auth() -> MockResponse {
    MockResponse::new(
        Headers::new(),
        StatusCode::Unauthorized,
        RawStatus(401, Cow::Borrowed("Unauthorized")),
        b"".to_vec()
    )
}

fn redirect_with_headers(host: String, mut headers: Headers) -> MockResponse {
    headers.set(Location(host.to_string()));

    MockResponse::new(
        headers,
        StatusCode::MovedPermanently,
        RawStatus(301, Cow::Borrowed("Moved Permanently")),
        b"".to_vec()
    )
}

enum ResponseType {
    Redirect(String),
    RedirectWithHeaders(String, Headers),
    Text(Vec<u8>),
    WithHeaders(Vec<u8>, Headers),
    NeedsAuth,
}

struct MockRequest {
    t: ResponseType
}

impl MockRequest {
    fn new(t: ResponseType) -> MockRequest {
        MockRequest { t: t }
    }
}

fn response_for_request_type(t: ResponseType) -> Result<MockResponse, LoadError> {
    match t {
        ResponseType::Redirect(location) => {
            Ok(redirect_to(location))
        },
        ResponseType::RedirectWithHeaders(location, headers) => {
            Ok(redirect_with_headers(location, headers))
        },
        ResponseType::Text(b) => {
            Ok(respond_with(b))
        },
        ResponseType::WithHeaders(b, h) => {
            Ok(respond_with_headers(b, h))
        },
        ResponseType::NeedsAuth => {
            Ok(basic_auth())
        }
    }
}

impl HttpRequest for MockRequest {
    type R = MockResponse;

    fn send(self, _: &Option<Vec<u8>>) -> Result<MockResponse, LoadError> {
        response_for_request_type(self.t)
    }
}

struct AssertMustHaveHeadersRequestFactory {
    expected_headers: Headers,
    body: Vec<u8>
}

impl HttpRequestFactory for AssertMustHaveHeadersRequestFactory {
    type R = MockRequest;

    fn create(&self, _: Url, _: Method, headers: Headers) -> Result<MockRequest, LoadError> {
        assert_eq!(headers, self.expected_headers);
        Ok(MockRequest::new(ResponseType::Text(self.body.clone())))
    }
}

struct AssertAuthHeaderRequestFactory {
    expected_headers: Headers,
    body: Vec<u8>
}

impl HttpRequestFactory for AssertAuthHeaderRequestFactory {
    type R = MockRequest;

    fn create(&self, _: Url, _: Method, headers: Headers) -> Result<MockRequest, LoadError> {
        let request = if headers.has::<Authorization<Basic>>() {
            assert_headers_included(&self.expected_headers, &headers);
            MockRequest::new(ResponseType::Text(self.body.clone()))
        } else {
            MockRequest::new(ResponseType::NeedsAuth)
        };

        Ok(request)
    }
}

fn assert_headers_included(expected: &Headers, request: &Headers) {
    assert!(expected.len() != 0);
    for header in expected.iter() {
        assert!(request.get_raw(header.name()).is_some());
        assert_eq!(request.get_raw(header.name()).unwrap(),
                   expected.get_raw(header.name()).unwrap())
    }
}

struct AssertMustIncludeHeadersRequestFactory {
    expected_headers: Headers,
    body: Vec<u8>
}

impl HttpRequestFactory for AssertMustIncludeHeadersRequestFactory {
    type R = MockRequest;

    fn create(&self, _: Url, _: Method, headers: Headers) -> Result<MockRequest, LoadError> {
        assert_headers_included(&self.expected_headers, &headers);
        Ok(MockRequest::new(ResponseType::Text(self.body.clone())))
    }
}

fn assert_cookie_for_domain(cookie_jar: Arc<RwLock<CookieStorage>>, domain: &str, cookie: &str) {
    let mut cookie_jar = cookie_jar.write().unwrap();
    let url = Url::parse(&*domain).unwrap();
    let cookies = cookie_jar.cookies_for_url(&url, CookieSource::HTTP);

    if let Some(cookie_list) = cookies {
        assert_eq!(cookie.to_owned(), cookie_list);
    } else {
        assert_eq!(cookie.len(), 0);
    }
}

struct AssertMustNotIncludeHeadersRequestFactory {
    headers_not_expected: Vec<String>,
    body: Vec<u8>
}

impl HttpRequestFactory for AssertMustNotIncludeHeadersRequestFactory {
    type R = MockRequest;

    fn create(&self, _: Url, _: Method, headers: Headers) -> Result<MockRequest, LoadError> {
        assert!(self.headers_not_expected.len() != 0);
        for header in &self.headers_not_expected {
            assert!(headers.get_raw(header).is_none());
        }

        Ok(MockRequest::new(ResponseType::Text(self.body.clone())))
    }
}

struct AssertMustHaveBodyRequest {
    expected_body: Option<Vec<u8>>,
    t: ResponseType
}

impl AssertMustHaveBodyRequest {
    fn new(t: ResponseType, expected_body: Option<Vec<u8>>) -> Self {
        AssertMustHaveBodyRequest { expected_body: expected_body, t: t }
    }
}

impl HttpRequest for AssertMustHaveBodyRequest {
    type R = MockResponse;

    fn send(self, body: &Option<Vec<u8>>) -> Result<MockResponse, LoadError> {
        assert_eq!(self.expected_body, *body);

        response_for_request_type(self.t)
    }
}

fn expect_devtools_http_request(devtools_port: &Receiver<DevtoolsControlMsg>) -> DevtoolsHttpRequest {
    match devtools_port.recv().unwrap() {
        DevtoolsControlMsg::FromChrome(
        ChromeToDevtoolsControlMsg::NetworkEvent(_, net_event)) => {
            match net_event {
                NetworkEvent::HttpRequest(httprequest) => {
                    httprequest
                },

                _ => panic!("No HttpRequest Received"),
            }
        },
        _ => panic!("No HttpRequest Received"),
    }
}

fn expect_devtools_http_response(devtools_port: &Receiver<DevtoolsControlMsg>) -> DevtoolsHttpResponse {
    match devtools_port.recv().unwrap() {
        DevtoolsControlMsg::FromChrome(
            ChromeToDevtoolsControlMsg::NetworkEvent(_, net_event_response)) => {
            match net_event_response {
                NetworkEvent::HttpResponse(httpresponse) => {
                    httpresponse
                },

                _ => panic!("No HttpResponse Received"),
            }
        },
        _ => panic!("No HttpResponse Received"),
    }
}

#[test]
fn test_check_default_headers_loaded_in_every_request() {
    let url = Url::parse("http://mozilla.com").unwrap();

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = None;
    load_data.method = Method::Get;

    let mut headers = Headers::new();
    headers.set(AcceptEncoding(vec![qitem(Encoding::Gzip),
                                    qitem(Encoding::Deflate),
                                    qitem(Encoding::EncodingExt("br".to_owned()))]));
    headers.set(Host { hostname: "mozilla.com".to_owned() , port: None });
    let accept = Accept(vec![
                            qitem(Mime(TopLevel::Text, SubLevel::Html, vec![])),
                            qitem(Mime(TopLevel::Application, SubLevel::Ext("xhtml+xml".to_owned()), vec![])),
                            QualityItem::new(Mime(TopLevel::Application, SubLevel::Xml, vec![]), Quality(900u16)),
                            QualityItem::new(Mime(TopLevel::Star, SubLevel::Star, vec![]), Quality(800u16)),
                            ]);
    headers.set(accept);
    headers.set(UserAgent(DEFAULT_USER_AGENT.to_owned()));

    // Testing for method.GET
    let _ = load(&load_data, &ui_provider, &http_state, None,
                 &AssertMustHaveHeadersRequestFactory {
                     expected_headers: headers.clone(),
                     body: <[_]>::to_vec(&[])
                 }, DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));

    // Testing for method.POST
    load_data.method = Method::Post;

    headers.set(ContentLength(0 as u64));

    let _ = load(&load_data, &ui_provider, &http_state, None,
                 &AssertMustHaveHeadersRequestFactory {
                     expected_headers: headers,
                     body: <[_]>::to_vec(&[])
                 }, DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));
}

#[test]
fn test_load_when_request_is_not_get_or_head_and_there_is_no_body_content_length_should_be_set_to_0() {
    let url = Url::parse("http://mozilla.com").unwrap();

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = None;
    load_data.method = Method::Post;

    let mut content_length = Headers::new();
    content_length.set(ContentLength(0));

    let _ = load(
        &load_data, &ui_provider, &http_state,
        None, &AssertMustIncludeHeadersRequestFactory {
            expected_headers: content_length,
            body: <[_]>::to_vec(&[])
        }, DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));
}

#[test]
fn test_request_and_response_data_with_network_messages() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let mut headers = Headers::new();
            headers.set(Host { hostname: "foo.bar".to_owned(), port: None });
            Ok(MockRequest::new(
                   ResponseType::WithHeaders(<[_]>::to_vec("Yay!".as_bytes()), headers))
            )
        }
    }

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let url = Url::parse("https://mozilla.com").unwrap();
    let (devtools_chan, devtools_port) = mpsc::channel::<DevtoolsControlMsg>();
    // This will probably have to be changed as it uses fake_root_pipeline_id which is marked for removal.
    let pipeline_id = PipelineId::fake_root_pipeline_id();
    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), Some(pipeline_id));
    let mut request_headers = Headers::new();
    request_headers.set(Host { hostname: "bar.foo".to_owned(), port: None });
    load_data.headers = request_headers.clone();
    let _ = load(&load_data, &ui_provider, &http_state, Some(devtools_chan), &Factory,
                 DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));

    // notification received from devtools
    let devhttprequest = expect_devtools_http_request(&devtools_port);
    let devhttpresponse = expect_devtools_http_response(&devtools_port);

    //Creating default headers for request
    let mut headers = Headers::new();
    headers.set(AcceptEncoding(vec![
                                   qitem(Encoding::Gzip),
                                   qitem(Encoding::Deflate),
                                   qitem(Encoding::EncodingExt("br".to_owned()))
                                   ]));
    headers.set(Host { hostname: "mozilla.com".to_owned() , port: None });
    let accept = Accept(vec![
                            qitem(Mime(TopLevel::Text, SubLevel::Html, vec![])),
                            qitem(Mime(TopLevel::Application, SubLevel::Ext("xhtml+xml".to_owned()), vec![])),
                            QualityItem::new(Mime(TopLevel::Application, SubLevel::Xml, vec![]), Quality(900u16)),
                            QualityItem::new(Mime(TopLevel::Star, SubLevel::Star, vec![]), Quality(800u16)),
                            ]);
    headers.set(accept);
    headers.set(UserAgent(DEFAULT_USER_AGENT.to_owned()));
    let httprequest = DevtoolsHttpRequest {
        url: url,
        method: Method::Get,
        headers: headers,
        body: None,
        pipeline_id: pipeline_id,
        startedDateTime: devhttprequest.startedDateTime
    };

    let content = "Yay!";
    let mut response_headers = Headers::new();
    response_headers.set(ContentLength(content.len() as u64));
    response_headers.set(Host { hostname: "foo.bar".to_owned(), port: None });

    let httpresponse = DevtoolsHttpResponse {
        headers: Some(response_headers),
        status: Some(RawStatus(200, Cow::Borrowed("Ok"))),
        body: None,
        pipeline_id: pipeline_id,
    };

    assert_eq!(devhttprequest, httprequest);
    assert_eq!(devhttpresponse, httpresponse);
}

#[test]
fn test_request_and_response_message_from_devtool_without_pipeline_id() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let mut headers = Headers::new();
            headers.set(Host { hostname: "foo.bar".to_owned(), port: None });
            Ok(MockRequest::new(
                   ResponseType::WithHeaders(<[_]>::to_vec("Yay!".as_bytes()), headers))
            )
        }
    }

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let url = Url::parse("https://mozilla.com").unwrap();
    let (devtools_chan, devtools_port) = mpsc::channel::<DevtoolsControlMsg>();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    let _ = load(&load_data, &ui_provider, &http_state, Some(devtools_chan), &Factory,
                 DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));

    // notification received from devtools
    assert!(devtools_port.try_recv().is_err());
}



#[test]
fn test_load_when_redirecting_from_a_post_should_rewrite_next_request_as_get() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, method: Method, _: Headers) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                assert_eq!(Method::Post, method);
                Ok(MockRequest::new(ResponseType::Redirect("http://mozilla.org".to_owned())))
            } else {
                assert_eq!(Method::Get, method);
                Ok(MockRequest::new(ResponseType::Text(<[_]>::to_vec("Yay!".as_bytes()))))
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.method = Method::Post;

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(&load_data, &ui_provider, &http_state, None, &Factory,
                 DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));
}

#[test]
fn test_load_should_decode_the_response_as_deflate_when_response_headers_have_content_encoding_deflate() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let mut e = DeflateEncoder::new(Vec::new(), Compression::Default);
            e.write(b"Yay!").unwrap();
            let encoded_content = e.finish().unwrap();

            let mut headers = Headers::new();
            headers.set(ContentEncoding(vec![Encoding::Deflate]));
            Ok(MockRequest::new(ResponseType::WithHeaders(encoded_content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let mut response = load(
        &load_data, &ui_provider, &http_state, None,
        &Factory,
        DEFAULT_USER_AGENT.to_owned(),
        &CancellationListener::new(None))
        .unwrap();

    assert_eq!(read_response(&mut response), "Yay!");
}

#[test]
fn test_load_should_decode_the_response_as_gzip_when_response_headers_have_content_encoding_gzip() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let mut e = GzEncoder::new(Vec::new(), Compression::Default);
            e.write(b"Yay!").unwrap();
            let encoded_content = e.finish().unwrap();

            let mut headers = Headers::new();
            headers.set(ContentEncoding(vec![Encoding::Gzip]));
            Ok(MockRequest::new(ResponseType::WithHeaders(encoded_content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let mut response = load(
        &load_data,
        &ui_provider, &http_state,
        None, &Factory,
        DEFAULT_USER_AGENT.to_owned(),
        &CancellationListener::new(None))
        .unwrap();

    assert_eq!(read_response(&mut response), "Yay!");
}

#[test]
fn test_load_doesnt_send_request_body_on_any_redirect() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = AssertMustHaveBodyRequest;

        fn create(&self, url: Url, _: Method, _: Headers) -> Result<AssertMustHaveBodyRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                Ok(
                    AssertMustHaveBodyRequest::new(
                        ResponseType::Redirect("http://mozilla.org".to_owned()),
                        Some(<[_]>::to_vec("Body on POST!".as_bytes()))
                    )
                )
            } else {
                Ok(
                    AssertMustHaveBodyRequest::new(
                        ResponseType::Text(<[_]>::to_vec("Yay!".as_bytes())),
                        None
                    )
                )
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Body on POST!".as_bytes()));

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(
        &load_data, &ui_provider, &http_state,
        None,
        &Factory,
        DEFAULT_USER_AGENT.to_owned(),
        &CancellationListener::new(None));
}

#[test]
fn test_load_doesnt_add_host_to_sts_list_when_url_is_http_even_if_sts_headers_are_present() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let content = <[_]>::to_vec("Yay!".as_bytes());
            let mut headers = Headers::new();
            headers.set(StrictTransportSecurity::excluding_subdomains(31536000));
            Ok(MockRequest::new(ResponseType::WithHeaders(content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();

    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &Factory,
                 DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));

    assert_eq!(http_state.hsts_list.read().unwrap().is_host_secure("mozilla.com"), false);
}

#[test]
fn test_load_adds_host_to_sts_list_when_url_is_https_and_sts_headers_are_present() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let content = <[_]>::to_vec("Yay!".as_bytes());
            let mut headers = Headers::new();
            headers.set(StrictTransportSecurity::excluding_subdomains(31536000));
            Ok(MockRequest::new(ResponseType::WithHeaders(content, headers)))
        }
    }

    let url = Url::parse("https://mozilla.com").unwrap();

    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &Factory,
                 DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));

    assert!(http_state.hsts_list.read().unwrap().is_host_secure("mozilla.com"));
}

#[test]
fn test_load_sets_cookies_in_the_resource_manager_when_it_get_set_cookie_header_in_response() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let content = <[_]>::to_vec("Yay!".as_bytes());
            let mut headers = Headers::new();
            headers.set(SetCookie(vec![CookiePair::new("mozillaIs".to_owned(), "theBest".to_owned())]));
            Ok(MockRequest::new(ResponseType::WithHeaders(content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    assert_cookie_for_domain(http_state.cookie_jar.clone(), "http://mozilla.com", "");

    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &Factory,
                 DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));

    assert_cookie_for_domain(http_state.cookie_jar.clone(), "http://mozilla.com", "mozillaIs=theBest");
}

#[test]
fn test_load_sets_requests_cookies_header_for_url_by_getting_cookies_from_the_resource_manager() {
    let url = Url::parse("http://mozilla.com").unwrap();

    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    {
        let mut cookie_jar = http_state.cookie_jar.write().unwrap();
        let cookie_url = url.clone();
        let cookie = Cookie::new_wrapped(
            CookiePair::new("mozillaIs".to_owned(), "theBest".to_owned()),
            &cookie_url,
            CookieSource::HTTP
        ).unwrap();
        cookie_jar.push(cookie, CookieSource::HTTP);
    }

    let mut cookie = Headers::new();
    cookie.set(CookieHeader(vec![CookiePair::new("mozillaIs".to_owned(), "theBest".to_owned())]));

    let _ = load(&load_data.clone(), &ui_provider, &http_state, None,
                 &AssertMustIncludeHeadersRequestFactory {
                     expected_headers: cookie,
                     body: <[_]>::to_vec(&*load_data.data.unwrap())
                 }, DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));
}

#[test]
fn test_load_sends_secure_cookie_if_http_changed_to_https_due_to_entry_in_hsts_store() {
    let url = Url::parse("http://mozilla.com").unwrap();
    let secured_url = Url::parse("https://mozilla.com").unwrap();
    let ui_provider = TestProvider::new();
    let http_state = HttpState::new();
    {
        let mut hsts_list = http_state.hsts_list.write().unwrap();
        let entry = HstsEntry::new(
            "mozilla.com".to_owned(), IncludeSubdomains::Included, Some(1000000)
        ).unwrap();
        hsts_list.push(entry);
    }

    {
        let mut cookie_jar = http_state.cookie_jar.write().unwrap();
        let cookie_url = secured_url.clone();
        let mut cookie_pair = CookiePair::new("mozillaIs".to_owned(), "theBest".to_owned());
        cookie_pair.secure = true;

        let cookie = Cookie::new_wrapped(
            cookie_pair,
            &cookie_url,
            CookieSource::HTTP
        ).unwrap();
        cookie_jar.push(cookie, CookieSource::HTTP);
    }

    let mut load_data = LoadData::new(LoadContext::Browsing, url, None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    let mut headers = Headers::new();
    headers.set_raw("Cookie".to_owned(), vec![<[_]>::to_vec("mozillaIs=theBest".as_bytes())]);

    let _ = load(
        &load_data.clone(), &ui_provider, &http_state, None,
        &AssertMustIncludeHeadersRequestFactory {
            expected_headers: headers,
            body: <[_]>::to_vec(&*load_data.data.unwrap())
        }, DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));
}

#[test]
fn test_load_sends_cookie_if_nonhttp() {
    let url = Url::parse("http://mozilla.com").unwrap();

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    {
        let mut cookie_jar = http_state.cookie_jar.write().unwrap();
        let cookie_url = url.clone();
        let cookie = Cookie::new_wrapped(
            CookiePair::new("mozillaIs".to_owned(), "theBest".to_owned()),
            &cookie_url,
            CookieSource::NonHTTP
        ).unwrap();
        cookie_jar.push(cookie, CookieSource::HTTP);
    }

    let mut load_data = LoadData::new(LoadContext::Browsing, url, None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    let mut headers = Headers::new();
    headers.set_raw("Cookie".to_owned(), vec![<[_]>::to_vec("mozillaIs=theBest".as_bytes())]);

    let _ = load(
        &load_data.clone(), &ui_provider, &http_state, None,
        &AssertMustIncludeHeadersRequestFactory {
            expected_headers: headers,
            body: <[_]>::to_vec(&*load_data.data.unwrap())
        }, DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));
}

#[test]
fn test_cookie_set_with_httponly_should_not_be_available_using_getcookiesforurl() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let content = <[_]>::to_vec("Yay!".as_bytes());
            let mut headers = Headers::new();
            headers.set_raw("set-cookie", vec![b"mozillaIs=theBest; HttpOnly;".to_vec()]);
            Ok(MockRequest::new(ResponseType::WithHeaders(content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &Factory,
                 DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));

    let mut cookie_jar = http_state.cookie_jar.write().unwrap();
    assert!(cookie_jar.cookies_for_url(&url, CookieSource::NonHTTP).is_none());
}

#[test]
fn test_when_cookie_received_marked_secure_is_ignored_for_http() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let content = <[_]>::to_vec("Yay!".as_bytes());
            let mut headers = Headers::new();
            headers.set_raw("set-cookie", vec![b"mozillaIs=theBest; Secure;".to_vec()]);
            Ok(MockRequest::new(ResponseType::WithHeaders(content, headers)))
        }
    }

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let load_data = LoadData::new(LoadContext::Browsing, Url::parse("http://mozilla.com").unwrap(), None);
    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &Factory,
                 DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));

    assert_cookie_for_domain(http_state.cookie_jar.clone(), "http://mozilla.com", "");
}

#[test]
fn test_when_cookie_set_marked_httpsonly_secure_isnt_sent_on_http_request() {

    let sec_url = Url::parse("https://mozilla.com").unwrap();
    let url = Url::parse("http://mozilla.com").unwrap();

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    {
        let mut cookie_jar = http_state.cookie_jar.write().unwrap();
        let cookie_url = sec_url.clone();
        let cookie = Cookie::new_wrapped(
            CookiePair::parse("mozillaIs=theBest; Secure;").unwrap(),
            &cookie_url,
            CookieSource::HTTP
        ).unwrap();
        cookie_jar.push(cookie, CookieSource::HTTP);
    }

    let mut load_data = LoadData::new(LoadContext::Browsing, url, None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    assert_cookie_for_domain(http_state.cookie_jar.clone(), "https://mozilla.com", "mozillaIs=theBest");

    let _ = load(
        &load_data.clone(), &ui_provider, &http_state, None,
        &AssertMustNotIncludeHeadersRequestFactory {
            headers_not_expected: vec!["Cookie".to_owned()],
            body: <[_]>::to_vec(&*load_data.data.unwrap())
        }, DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));
}

#[test]
fn test_load_sets_content_length_to_length_of_request_body() {
    let content = "This is a request body";

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = Some(<[_]>::to_vec(content.as_bytes()));

    let mut content_len_headers = Headers::new();
    content_len_headers.set(ContentLength(content.as_bytes().len() as u64));

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(&load_data.clone(), &ui_provider, &http_state,
                 None, &AssertMustIncludeHeadersRequestFactory {
                     expected_headers: content_len_headers,
                     body: <[_]>::to_vec(&*load_data.data.unwrap())
                 }, DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));
}

#[test]
fn test_load_uses_explicit_accept_from_headers_in_load_data() {
    let text_html = qitem(Mime(TopLevel::Text, SubLevel::Html, vec![]));

    let mut accept_headers = Headers::new();
    accept_headers.set(Accept(vec![text_html.clone()]));

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));
    load_data.headers.set(Accept(vec![text_html.clone()]));

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &AssertMustIncludeHeadersRequestFactory {
                     expected_headers: accept_headers,
                     body: <[_]>::to_vec("Yay!".as_bytes())
                 }, DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));
}

#[test]
fn test_load_sets_default_accept_to_html_xhtml_xml_and_then_anything_else() {
    let mut accept_headers = Headers::new();
    accept_headers.set(Accept(vec![
        qitem(Mime(TopLevel::Text, SubLevel::Html, vec![])),
        qitem(Mime(TopLevel::Application, SubLevel::Ext("xhtml+xml".to_owned()), vec![])),
        QualityItem::new(Mime(TopLevel::Application, SubLevel::Xml, vec![]), Quality(900)),
        QualityItem::new(Mime(TopLevel::Star, SubLevel::Star, vec![]), Quality(800)),
    ]));

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &AssertMustIncludeHeadersRequestFactory {
                     expected_headers: accept_headers,
                     body: <[_]>::to_vec("Yay!".as_bytes())
                 }, DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));
}

#[test]
fn test_load_uses_explicit_accept_encoding_from_load_data_headers() {
    let mut accept_encoding_headers = Headers::new();
    accept_encoding_headers.set(AcceptEncoding(vec![qitem(Encoding::Chunked)]));

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));
    load_data.headers.set(AcceptEncoding(vec![qitem(Encoding::Chunked)]));

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &AssertMustIncludeHeadersRequestFactory {
                     expected_headers: accept_encoding_headers,
                     body: <[_]>::to_vec("Yay!".as_bytes())
                 }, DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));
}

#[test]
fn test_load_sets_default_accept_encoding_to_gzip_and_deflate() {
    let mut accept_encoding_headers = Headers::new();
    accept_encoding_headers.set(AcceptEncoding(vec![qitem(Encoding::Gzip),
                                                    qitem(Encoding::Deflate),
                                                    qitem(Encoding::EncodingExt("br".to_owned()))]));

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let _ = load(&load_data,
                 &ui_provider, &http_state,
                 None,
                 &AssertMustIncludeHeadersRequestFactory {
                     expected_headers: accept_encoding_headers,
                     body: <[_]>::to_vec("Yay!".as_bytes())
                 }, DEFAULT_USER_AGENT.to_owned(),
                 &CancellationListener::new(None));
}

#[test]
fn test_load_errors_when_there_a_redirect_loop() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                Ok(MockRequest::new(ResponseType::Redirect("http://mozilla.org".to_owned())))
            } else if url.domain().unwrap() == "mozilla.org" {
                Ok(MockRequest::new(ResponseType::Redirect("http://mozilla.com".to_owned())))
            } else {
                panic!("unexpected host {:?}", url)
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    match load(&load_data, &ui_provider, &http_state, None, &Factory,
               DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None)) {
        Err(ref load_err) if load_err.error == LoadErrorType::InvalidRedirect =>
            assert_eq!(&load_err.reason, "redirect loop"),
        _ => panic!("expected max redirects to fail")
    }
}

#[test]
fn test_load_errors_when_there_is_too_many_redirects() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                Ok(MockRequest::new(ResponseType::Redirect(format!("{}/1", url))))
            } else {
                panic!("unexpected host {:?}", url)
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let redirect_limit = 13.;
    prefs::set_pref("network.http.redirection-limit",
                    prefs::PrefValue::Number(redirect_limit));

    match load(&load_data, &ui_provider, &http_state, None, &Factory,
               DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None)) {
        Err(LoadError { error: LoadErrorType::MaxRedirects(num_redirects),
                        url, .. }) => {
            assert_eq!(num_redirects, redirect_limit as u32);
            assert_eq!(url.domain().unwrap(), "mozilla.com");
        }
        _ => panic!("expected max redirects to fail")
    }

    prefs::reset_pref("network.http.redirection-limit");
}

#[test]
fn test_load_follows_a_redirect() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                Ok(MockRequest::new(ResponseType::Redirect("http://mozilla.org".to_owned())))
            } else if url.domain().unwrap() == "mozilla.org" {
                Ok(
                    MockRequest::new(
                        ResponseType::Text(
                            <[_]>::to_vec("Yay!".as_bytes())
                        )
                    )
                )
            } else {
                panic!("unexpected host {:?}", url)
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    match load(&load_data, &ui_provider, &http_state, None, &Factory,
               DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None)) {
        Err(e) => panic!("expected to follow a redirect {:?}", e),
        Ok(mut lr) => {
            let response = read_response(&mut lr);
            assert_eq!(response, "Yay!".to_owned());
        }
    }
}

struct DontConnectFactory;

impl HttpRequestFactory for DontConnectFactory {
    type R = MockRequest;

    fn create(&self, url: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
        Err(LoadError::new(url, LoadErrorType::Connection, "should not have connected".to_owned()))
    }
}

#[test]
fn test_load_errors_when_scheme_is_not_http_or_https() {
    let url = Url::parse("ftp://not-supported").unwrap();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    match load(&load_data,
               &ui_provider, &http_state,
               None,
               &DontConnectFactory,
               DEFAULT_USER_AGENT.to_owned(),
               &CancellationListener::new(None)) {
        Err(ref load_err) if load_err.error == LoadErrorType::UnsupportedScheme => (),
        _ => panic!("expected ftp scheme to be unsupported")
    }
}

#[test]
fn test_load_errors_when_viewing_source_and_inner_url_scheme_is_not_http_or_https() {
    let url = Url::parse("view-source:ftp://not-supported").unwrap();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    match load(&load_data,
               &ui_provider, &http_state,
               None,
               &DontConnectFactory,
               DEFAULT_USER_AGENT.to_owned(),
               &CancellationListener::new(None)) {
        Err(ref load_err) if load_err.error == LoadErrorType::UnsupportedScheme => (),
        _ => panic!("expected ftp scheme to be unsupported")
    }
}

#[test]
fn test_load_errors_when_cancelled() {
    use ipc_channel::ipc;
    use net::resource_thread::CancellableResource;
    use net_traits::ResourceId;

    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method, _: Headers) -> Result<MockRequest, LoadError> {
            let mut headers = Headers::new();
            headers.set(Host { hostname: "Kaboom!".to_owned(), port: None });
            Ok(MockRequest::new(
                   ResponseType::WithHeaders(<[_]>::to_vec("BOOM!".as_bytes()), headers))
            )
        }
    }

    let (id_sender, _id_receiver) = ipc::channel().unwrap();
    let (cancel_sender, cancel_receiver) = mpsc::channel();
    let cancel_resource = CancellableResource::new(cancel_receiver, ResourceId(0), id_sender);
    let cancel_listener = CancellationListener::new(Some(cancel_resource));
    cancel_sender.send(()).unwrap();

    let url = Url::parse("https://mozilla.com").unwrap();
    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);
    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    match load(&load_data,
               &ui_provider, &http_state,
               None,
               &Factory,
               DEFAULT_USER_AGENT.to_owned(),
               &cancel_listener) {
        Err(ref load_err) if load_err.error == LoadErrorType::Cancelled => (),
        _ => panic!("expected load cancelled error!")
    }
}

#[test]
fn  test_redirect_from_x_to_y_provides_y_cookies_from_y() {
    let url_x = Url::parse("http://mozilla.com").unwrap();
    let url_y = Url::parse("http://mozilla.org").unwrap();

    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, _: Method, headers: Headers) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                let mut expected_headers_x = Headers::new();
                expected_headers_x.set_raw("Cookie".to_owned(),
                    vec![<[_]>::to_vec("mozillaIsNot=dotCom".as_bytes())]);
                assert_headers_included(&expected_headers_x, &headers);

                Ok(MockRequest::new(
                    ResponseType::Redirect("http://mozilla.org".to_owned())))
            } else if url.domain().unwrap() == "mozilla.org" {
                let mut expected_headers_y = Headers::new();
                expected_headers_y.set_raw(
                    "Cookie".to_owned(),
                    vec![<[_]>::to_vec("mozillaIs=theBest".as_bytes())]);
                assert_headers_included(&expected_headers_y, &headers);

                Ok(MockRequest::new(
                    ResponseType::Text(<[_]>::to_vec("Yay!".as_bytes()))))
            } else {
                panic!("unexpected host {:?}", url)
            }
        }
    }

    let load_data = LoadData::new(LoadContext::Browsing, url_x.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    {
        let mut cookie_jar = http_state.cookie_jar.write().unwrap();
        let cookie_x_url = url_x.clone();
        let cookie_x = Cookie::new_wrapped(
            CookiePair::new("mozillaIsNot".to_owned(), "dotCom".to_owned()),
            &cookie_x_url,
            CookieSource::HTTP
        ).unwrap();

        cookie_jar.push(cookie_x, CookieSource::HTTP);

        let cookie_y_url = url_y.clone();
        let cookie_y = Cookie::new_wrapped(
            CookiePair::new("mozillaIs".to_owned(), "theBest".to_owned()),
            &cookie_y_url,
            CookieSource::HTTP
        ).unwrap();
        cookie_jar.push(cookie_y, CookieSource::HTTP);
    }

    match load(&load_data,
               &ui_provider, &http_state,
               None,
               &Factory,
               DEFAULT_USER_AGENT.to_owned(),
               &CancellationListener::new(None)) {
        Err(e) => panic!("expected to follow a redirect {:?}", e),
        Ok(mut lr) => {
            let response = read_response(&mut lr);
            assert_eq!(response, "Yay!".to_owned());
        }
    }
}

#[test]
fn test_redirect_from_x_to_x_provides_x_with_cookie_from_first_response() {
    let url = Url::parse("http://mozilla.org/initial/").unwrap();

    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, _: Method, headers: Headers) -> Result<MockRequest, LoadError> {
            if url.path_segments().unwrap().next().unwrap() == "initial" {
                let mut initial_answer_headers = Headers::new();
                initial_answer_headers.set_raw("set-cookie", vec![b"mozillaIs=theBest; path=/;".to_vec()]);
                Ok(MockRequest::new(
                    ResponseType::RedirectWithHeaders("http://mozilla.org/subsequent/".to_owned(),
                        initial_answer_headers)))
            } else if url.path_segments().unwrap().next().unwrap() == "subsequent" {
                let mut expected_subsequent_headers = Headers::new();
                expected_subsequent_headers.set_raw("Cookie", vec![b"mozillaIs=theBest".to_vec()]);
                assert_headers_included(&expected_subsequent_headers, &headers);
                Ok(MockRequest::new(ResponseType::Text(b"Yay!".to_vec())))
            } else {
                panic!("unexpected host {:?}", url)
            }
        }
    }

    let load_data = LoadData::new(LoadContext::Browsing, url.clone(), None);

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    match load(&load_data,
               &ui_provider, &http_state,
               None,
               &Factory,
               DEFAULT_USER_AGENT.to_owned(),
               &CancellationListener::new(None)) {
        Err(e) => panic!("expected to follow a redirect {:?}", e),
        Ok(mut lr) => {
            let response = read_response(&mut lr);
            assert_eq!(response, "Yay!".to_owned());
        }
    }
}

#[test]
fn test_if_auth_creds_not_in_url_but_in_cache_it_sets_it() {
    let url = Url::parse("http://mozilla.com").unwrap();

    let http_state = HttpState::new();
    let ui_provider = TestProvider::new();

    let auth_entry = AuthCacheEntry {
                        user_name: "username".to_owned(),
                        password: "test".to_owned(),
                     };

    http_state.auth_cache.write().unwrap().entries.insert(url.clone(), auth_entry);

    let mut load_data = LoadData::new(LoadContext::Browsing, url, None);
    load_data.credentials_flag = true;

    let mut auth_header = Headers::new();

    auth_header.set(
       Authorization(
           Basic {
               username: "username".to_owned(),
               password: Some("test".to_owned())
           }
       )
    );

    let _ = load(
        &load_data, &ui_provider, &http_state,
        None, &AssertMustIncludeHeadersRequestFactory {
            expected_headers: auth_header,
            body: <[_]>::to_vec(&[])
        }, DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None));
}

#[test]
fn test_auth_ui_sets_header_on_401() {
    let url = Url::parse("http://mozilla.com").unwrap();
    let http_state = HttpState::new();
    let ui_provider = TestProvider { username: "test".to_owned(), password: "test".to_owned() };

    let mut auth_header = Headers::new();

    auth_header.set(
       Authorization(
           Basic {
               username: "test".to_owned(),
               password: Some("test".to_owned())
           }
       )
    );

    let load_data = LoadData::new(LoadContext::Browsing, url, None);

    match load(
        &load_data, &ui_provider, &http_state,
        None, &AssertAuthHeaderRequestFactory {
            expected_headers: auth_header,
            body: <[_]>::to_vec(&[])
        }, DEFAULT_USER_AGENT.to_owned(), &CancellationListener::new(None)) {
        Err(e) => panic!("response contained error {:?}", e),
        Ok(response) => {
            assert_eq!(response.metadata.status, Some(RawStatus(200, Cow::Borrowed("Ok"))));
        }
    }
}
