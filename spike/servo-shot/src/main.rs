//! Spike: render a pre-processed (JS-off) page with Servo and save a PNG.
//!
//! usage: servo-shot <html-file> <real-url> <out.png> [block-host]
//!
//! The main document request is intercepted and answered with <html-file>, so Servo
//! renders exactly the same HTML Blitz gets. Every other request is logged, and any
//! request to <block-host> (or any script) is refused, to test the network chokepoint.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use servo::{
    EventLoopWaker, LoadStatus, RenderingContext, ServoBuilder, SoftwareRenderingContext,
    WebResourceLoad, WebResourceResponse, WebView, WebViewBuilder, WebViewDelegate,
};
use url::Url;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 1400;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct NoopWaker;
impl EventLoopWaker for NoopWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }
    // We poll spin_event_loop in a loop, so there is nothing to wake.
    fn wake(&self) {}
}

struct Delegate {
    html: Vec<u8>,
    block_host: Option<String>,
    out_path: String,
    load_complete: Cell<bool>,
    frame_ready: Cell<bool>,
    done: Rc<Cell<bool>>,
    requests: RefCell<Vec<String>>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _: WebView) {
        self.frame_ready.set(true);
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        if status != LoadStatus::Complete || self.load_complete.replace(true) {
            return;
        }
        let out_path = self.out_path.clone();
        let done = self.done.clone();
        webview.take_screenshot(None, move |result| {
            match result {
                Ok(image) => image.save(&out_path).expect("could not write png"),
                Err(error) => eprintln!("SCREENSHOT_ERROR {error:?}"),
            }
            done.set(true);
        });
    }

    fn load_web_resource(&self, _: WebView, load: WebResourceLoad) {
        let request = load.request().clone();
        let url = request.url.clone();
        let destination = format!("{:?}", request.destination);

        if request.is_for_main_frame && destination == "Document" {
            self.requests.borrow_mut().push(format!("SERVE    {destination:10} {url}"));
            let mut headers = http::HeaderMap::new();
            headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
            let mut intercepted = load.intercept(WebResourceResponse::new(url).headers(headers));
            intercepted.send_body_data(self.html.clone());
            intercepted.finish();
            return;
        }

        let blocked_host = self.block_host.as_deref().is_some_and(|host| url.host_str() == Some(host));
        if blocked_host || destination == "Script" {
            self.requests.borrow_mut().push(format!("BLOCKED  {destination:10} {url}"));
            let response = WebResourceResponse::new(url).status_code(http::StatusCode::FORBIDDEN);
            load.intercept(response).finish();
            return;
        }

        self.requests.borrow_mut().push(format!("ALLOWED  {destination:10} {url}"));
        // Dropping `load` without intercepting lets Servo fetch it normally.
    }
}

fn main() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install crypto provider");

    let args: Vec<String> = std::env::args().collect();
    let [_, html_path, real_url, out_path, rest @ ..] = args.as_slice() else {
        eprintln!("usage: servo-shot <html-file> <real-url> <out.png> [block-host]");
        std::process::exit(2);
    };
    let started = Instant::now();

    let rendering_context = Rc::new(
        SoftwareRenderingContext::new(dpi::PhysicalSize::new(WIDTH, HEIGHT))
            .expect("Could not create software rendering context"),
    );
    rendering_context.make_current().expect("make_current failed");

    let servo = ServoBuilder::default().event_loop_waker(Box::new(NoopWaker)).build();

    let done = Rc::new(Cell::new(false));
    let delegate = Rc::new(Delegate {
        html: std::fs::read(html_path).expect("could not read html"),
        block_host: rest.first().cloned(),
        out_path: out_path.clone(),
        load_complete: Cell::new(false),
        frame_ready: Cell::new(false),
        done: done.clone(),
        requests: RefCell::new(Vec::new()),
    });

    let webview = WebViewBuilder::new(&servo, rendering_context.clone())
        .url(Url::parse(real_url).expect("bad url"))
        .delegate(delegate.clone())
        .build();

    while !done.get() {
        servo.spin_event_loop();
        if delegate.frame_ready.replace(false) {
            webview.paint();
        }
        if started.elapsed() > TIMEOUT {
            eprintln!("TIMEOUT after {:?}", TIMEOUT);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    for line in delegate.requests.borrow().iter() {
        println!("{line}");
    }
    println!("ELAPSED_MS {}", started.elapsed().as_millis());
}
