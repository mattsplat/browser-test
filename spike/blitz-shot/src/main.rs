//! Spike: render a pre-processed (JS-off) page with Blitz and save a PNG.
//!
//! usage: blitz-shot <html-file> <real-url> <out.png> [block-host]
//!
//! Same inputs and policy as servo-shot: every subresource request goes through
//! `GatedProvider`, which logs it and refuses anything to <block-host>.

use std::fs::File;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyrender::{PaintScene as _, render_to_buffer};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{DocumentConfig, util::Color};
use blitz_html::HtmlDocument;
use blitz_net::Provider;
use blitz_paint::paint_scene;
use blitz_traits::net::{NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::Fill;
use peniko::kurbo::Rect;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 1400;
const TIMEOUT: Duration = Duration::from_secs(30);

/// The network chokepoint: wraps the real provider so every fetch is logged and can be refused.
struct GatedProvider {
    inner: Arc<Provider>,
    block_host: Option<String>,
    log: Mutex<Vec<String>>,
}

impl NetProvider for GatedProvider {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url.clone();
        if self.block_host.as_deref().is_some_and(|host| url.host_str() == Some(host)) {
            self.log.lock().unwrap().push(format!("BLOCKED  {url}"));
            handler.error(url.to_string(), "blocked by policy".into());
            return;
        }
        self.log.lock().unwrap().push(format!("ALLOWED  {url}"));
        self.inner.fetch(doc_id, request, handler);
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, html_path, real_url, out_path, rest @ ..] = args.as_slice() else {
        eprintln!("usage: blitz-shot <html-file> <real-url> <out.png> [block-host]");
        std::process::exit(2);
    };
    let started = Instant::now();
    let html = std::fs::read_to_string(html_path).expect("could not read html");

    let inner = Arc::new(Provider::new(None));
    let net = Arc::new(GatedProvider {
        inner: inner.clone(),
        block_host: rest.first().cloned(),
        log: Mutex::new(Vec::new()),
    });

    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some(real_url.clone()),
            net_provider: Some(net.clone() as _),
            viewport: Some(Viewport::new(WIDTH, HEIGHT, 1.0, ColorScheme::Light)),
            ..Default::default()
        },
    );

    // Pump the document until all subresources (CSS, images, fonts) have arrived.
    loop {
        document.resolve(0.0);
        if inner.is_empty() || started.elapsed() > TIMEOUT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if started.elapsed() > TIMEOUT {
        eprintln!("TIMEOUT after {:?}", TIMEOUT);
    }
    document.as_mut().resolve(0.0);

    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            let full = Rect::new(0.0, 0.0, WIDTH as f64, HEIGHT as f64);
            scene.fill(Fill::NonZero, Default::default(), Color::WHITE, Default::default(), &full);
            paint_scene(scene, document.as_mut(), 1.0, WIDTH, HEIGHT, 0, 0);
        },
        WIDTH,
        HEIGHT,
    );

    let mut encoder = png::Encoder::new(File::create(out_path).unwrap(), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&buffer).unwrap();
    writer.finish().unwrap();

    for line in net.log.lock().unwrap().iter() {
        println!("{line}");
    }
    println!("ELAPSED_MS {}", started.elapsed().as_millis());
}
