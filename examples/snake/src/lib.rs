//! Snake on a 20 × 20 grid: steer with the arrow keys or WASD, eat the food to grow, don't hit
//! the walls or yourself. It speeds up as you score; the best score is kept in localStorage.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use unscripted::{Object, document, random, set_timeout, window};

const CELLS: i32 = 20;
const CELL: f64 = 20.0;
const BEST_KEY: &str = "snake-best";

type Point = (i32, i32);

struct Game {
    snake: VecDeque<Point>,
    /// The way it's moving, and the way the last key asked for (applied on the next step,
    /// so two quick keys can't turn it back into itself).
    heading: Point,
    wanted: Point,
    food: Point,
    score: u32,
    best: u32,
    alive: bool,
    paused: bool,
    /// Bumped by each new game, so the old game's pending step stops itself.
    round: u32,
}

impl Game {
    fn new(best: u32, round: u32) -> Game {
        let snake: VecDeque<Point> = [(7, 10), (6, 10), (5, 10)].into_iter().collect();
        let mut game = Game { snake, heading: (1, 0), wanted: (1, 0), food: (0, 0), score: 0, best, alive: true, paused: false, round };
        game.place_food();
        game
    }

    /// Food on a free cell, picked with the browser's random numbers.
    fn place_food(&mut self) {
        loop {
            let cell = ((random() * CELLS as f64) as i32, (random() * CELLS as f64) as i32);
            if !self.snake.contains(&cell) {
                self.food = cell;
                return;
            }
        }
    }

    fn steer(&mut self, to: Point) {
        // No reversing into your own neck.
        if (to.0 + self.heading.0, to.1 + self.heading.1) != (0, 0) {
            self.wanted = to;
        }
    }

    /// One move. Returns false when the snake dies.
    fn step(&mut self) -> bool {
        self.heading = self.wanted;
        let head = self.snake[0];
        let next = (head.0 + self.heading.0, head.1 + self.heading.1);
        let eats = next == self.food;
        // The tail moves out of the way unless the snake grows this step.
        let body = if eats { self.snake.len() } else { self.snake.len() - 1 };
        let hits_self = self.snake.iter().take(body).any(|&p| p == next);
        if !(0..CELLS).contains(&next.0) || !(0..CELLS).contains(&next.1) || hits_self {
            self.alive = false;
            return false;
        }
        self.snake.push_front(next);
        if eats {
            self.score += 1;
            self.best = self.best.max(self.score);
            self.place_food();
        } else {
            self.snake.pop_back();
        }
        true
    }

    /// Milliseconds between steps: faster as the score grows.
    fn delay(&self) -> u32 {
        140u32.saturating_sub(self.score * 4).max(60)
    }

    fn draw(&self, ctx: &Object) {
        let rect = |x: f64, y: f64, w: f64, h: f64| {
            let _ = ctx.call("fillRect", &[x.into(), y.into(), w.into(), h.into()]);
        };
        let fill = |color: &str| {
            let _ = ctx.set("fillStyle", color);
        };
        let size = CELLS as f64 * CELL;
        fill("#10151C");
        rect(0.0, 0.0, size, size);
        fill("#E5484D");
        rect(self.food.0 as f64 * CELL + 3.0, self.food.1 as f64 * CELL + 3.0, CELL - 6.0, CELL - 6.0);
        for (i, &(x, y)) in self.snake.iter().enumerate() {
            fill(if i == 0 { "#9BE564" } else { "#46A758" });
            rect(x as f64 * CELL + 1.0, y as f64 * CELL + 1.0, CELL - 2.0, CELL - 2.0);
        }
        if !self.alive || self.paused {
            fill("#FFFFFF");
            let _ = ctx.set("font", "28px sans-serif");
            let text = if self.alive { "Paused" } else { "Game over" };
            let _ = ctx.call("fillText", &[text.into(), (size / 2.0 - 70.0).into(), (size / 2.0).into()]);
        }
    }
}

struct App {
    game: Game,
    ctx: Object,
    status: Object,
    score: Object,
    best: Object,
}

impl App {
    fn show(&self) {
        self.game.draw(&self.ctx);
        self.score.set_text(&self.game.score.to_string());
        self.best.set_text(&self.game.best.to_string());
        let status = match (self.game.alive, self.game.paused) {
            (false, _) => "Game over. Press Enter for another.",
            (true, true) => "Paused. Space to carry on.",
            (true, false) => "Go!",
        };
        self.status.set_text(status);
    }
}

/// The game loop: one step, then the next after the current delay. A step from an old round
/// (before "New game") or while paused does nothing.
fn schedule(app: Rc<RefCell<App>>, round: u32) {
    let delay = app.borrow().game.delay();
    set_timeout(
        move || {
            let mut a = app.borrow_mut();
            if a.game.round != round || !a.game.alive {
                return;
            }
            if !a.game.paused && !a.game.step() {
                if let Ok(storage) = window().get("localStorage") {
                    if let Some(storage) = storage.as_object() {
                        let _ = storage.call("setItem", &[BEST_KEY.into(), a.game.best.to_string().into()]);
                    }
                }
            }
            a.show();
            drop(a);
            schedule(app, round);
        },
        delay,
    );
}

fn new_game(app: &Rc<RefCell<App>>) {
    let mut a = app.borrow_mut();
    let round = a.game.round + 1;
    a.game = Game::new(a.game.best, round);
    a.show();
    drop(a);
    schedule(app.clone(), round);
}

fn saved_best() -> u32 {
    let storage = window().object("localStorage");
    let saved = storage.and_then(|s| s.call("getItem", &[BEST_KEY.into()]).ok());
    saved.and_then(|v| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0)
}

fn start() {
    let page = document();
    let canvas = page.query("#board").unwrap();
    let ctx = canvas.call("getContext", &["2d".into()]).unwrap().as_object().cloned().unwrap();
    let app = Rc::new(RefCell::new(App {
        game: Game::new(saved_best(), 0),
        ctx,
        status: page.query("#status").unwrap(),
        score: page.query("#score").unwrap(),
        best: page.query("#best").unwrap(),
    }));

    let keys = app.clone();
    page.on("keydown", move |event: Object| {
        let turn = match event.text_of("key").as_str() {
            "ArrowUp" | "w" | "W" => Some((0, -1)),
            "ArrowDown" | "s" | "S" => Some((0, 1)),
            "ArrowLeft" | "a" | "A" => Some((-1, 0)),
            "ArrowRight" | "d" | "D" => Some((1, 0)),
            _ => None,
        };
        match (turn, event.text_of("key").as_str()) {
            (Some(to), _) => keys.borrow_mut().game.steer(to),
            (None, " ") => {
                let mut a = keys.borrow_mut();
                a.game.paused = !a.game.paused && a.game.alive;
                a.show();
            }
            (None, "Enter") => new_game(&keys),
            _ => return,
        }
        // The page shouldn't scroll on arrow keys or Space.
        let _ = event.call("preventDefault", &[]);
    });
    let restart = app.clone();
    page.query("#restart").unwrap().on("click", move |_: Object| new_game(&restart));

    new_game(&app);
}

unscripted::main!(start);
