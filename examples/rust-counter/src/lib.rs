//! The same counter as the Dart, JS, Python and PHP examples, in Rust.

use std::cell::Cell;
use std::rc::Rc;

use unscripted::{Object, create, document};

fn start() {
    let page = document();
    page.query("#status").unwrap().set_text("Rust is running, as a pack in WebAssembly.");
    let count = page.query("#count").unwrap();
    let log = page.query("#log").unwrap();
    let clicks = Rc::new(Cell::new(0));

    page.query("#increment").unwrap().on("click", move |event: Object| {
        clicks.set(clicks.get() + 1);
        count.set_text(&clicks.get().to_string());
        let item = create("li");
        item.set_text(&format!("Click {}, handled in Rust ({})", clicks.get(), event.text_of("type")));
        log.append(&item);
        println!("click {}", clicks.get());
    });
}

unscripted::main!(start);
