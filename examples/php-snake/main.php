<?php
// Snake on a 20 × 20 grid: steer with the arrow keys or WASD, eat the food to grow, don't hit
// the walls or yourself. It speeds up as you score; the best score is kept in localStorage.
// The same game as examples/snake, in PHP.

use Unscripted as U;

const CELLS = 20;
const CELL = 20;
const BEST_KEY = 'snake-best';

final class Game {
    /** @var array<int, array{int, int}> head first */
    public array $snake = [[7, 10], [6, 10], [5, 10]];
    /** The way it's moving, and the way the last key asked for (applied on the next step, so
     * two quick keys can't turn it back into itself). */
    public array $heading = [1, 0];
    public array $wanted = [1, 0];
    public array $food = [0, 0];
    public int $score = 0;
    public bool $alive = true;
    public bool $paused = false;

    public function __construct(public int $best) {
        $this->placeFood();
    }

    /** Food on a free cell. */
    private function placeFood(): void {
        do {
            $cell = [mt_rand(0, CELLS - 1), mt_rand(0, CELLS - 1)];
        } while (in_array($cell, $this->snake, true));
        $this->food = $cell;
    }

    public function steer(array $to): void {
        // No reversing into your own neck.
        if ($to[0] + $this->heading[0] !== 0 || $to[1] + $this->heading[1] !== 0) {
            $this->wanted = $to;
        }
    }

    /** One move. Returns false when the snake dies. */
    public function step(): bool {
        $this->heading = $this->wanted;
        $next = [$this->snake[0][0] + $this->heading[0], $this->snake[0][1] + $this->heading[1]];
        $eats = $next === $this->food;
        // The tail moves out of the way unless the snake grows this step.
        $body = $eats ? $this->snake : array_slice($this->snake, 0, -1);
        $outside = $next[0] < 0 || $next[0] >= CELLS || $next[1] < 0 || $next[1] >= CELLS;
        if ($outside || in_array($next, $body, true)) {
            $this->alive = false;
            return false;
        }
        array_unshift($this->snake, $next);
        if ($eats) {
            $this->score++;
            $this->best = max($this->best, $this->score);
            $this->placeFood();
        } else {
            array_pop($this->snake);
        }
        return true;
    }

    /** Milliseconds between steps: faster as the score grows. */
    public function delay(): int {
        return max(60, 140 - $this->score * 4);
    }

    public function draw(object $ctx): void {
        $size = CELLS * CELL;
        $ctx->fillStyle = '#10151C';
        $ctx->fillRect(0, 0, $size, $size);
        $ctx->fillStyle = '#E5484D';
        $ctx->fillRect($this->food[0] * CELL + 3, $this->food[1] * CELL + 3, CELL - 6, CELL - 6);
        foreach ($this->snake as $i => [$x, $y]) {
            $ctx->fillStyle = $i === 0 ? '#9BE564' : '#46A758';
            $ctx->fillRect($x * CELL + 1, $y * CELL + 1, CELL - 2, CELL - 2);
        }
        if (!$this->alive || $this->paused) {
            $ctx->fillStyle = '#FFFFFF';
            $ctx->font = '28px sans-serif';
            $ctx->fillText($this->alive ? 'Paused' : 'Game over', $size / 2 - 70, $size / 2);
        }
    }
}

final class App {
    public Game $game;
    /** Bumped by each new game, so the old game's pending step stops itself. */
    private int $round = 0;
    private object $ctx;

    public function __construct() {
        $this->ctx = U::document()->querySelector('#board')->getContext('2d');
        $this->game = new Game((int) U::localStorage()->getItem(BEST_KEY));
    }

    public function show(): void {
        $document = U::document();
        $this->game->draw($this->ctx);
        $document->querySelector('#score')->textContent = (string) $this->game->score;
        $document->querySelector('#best')->textContent = (string) $this->game->best;
        $document->querySelector('#status')->textContent = match (true) {
            !$this->game->alive => 'Game over. Press Enter for another.',
            $this->game->paused => 'Paused. Space to carry on.',
            default => 'Go!',
        };
    }

    public function newGame(): void {
        $this->round++;
        $this->game = new Game($this->game->best);
        $this->show();
        $this->schedule($this->round);
    }

    /** The game loop: one step, then the next after the current delay. A step from an old
     * round (before "New game") or while paused does nothing. */
    private function schedule(int $round): void {
        U::setTimeout(function () use ($round) {
            if ($round !== $this->round || !$this->game->alive) {
                return;
            }
            if (!$this->game->paused && !$this->game->step()) {
                U::localStorage()->setItem(BEST_KEY, (string) $this->game->best);
            }
            $this->show();
            $this->schedule($round);
        }, $this->game->delay());
    }

    public function key($event): void {
        $turn = match ($event->key) {
            'ArrowUp', 'w', 'W' => [0, -1],
            'ArrowDown', 's', 'S' => [0, 1],
            'ArrowLeft', 'a', 'A' => [-1, 0],
            'ArrowRight', 'd', 'D' => [1, 0],
            default => null,
        };
        if ($turn !== null) {
            $this->game->steer($turn);
        } elseif ($event->key === ' ') {
            $this->game->paused = !$this->game->paused && $this->game->alive;
            $this->show();
        } elseif ($event->key === 'Enter') {
            $this->newGame();
        } else {
            return;
        }
        // The page shouldn't scroll on arrow keys or Space.
        $event->preventDefault();
    }
}

$app = new App();
U::document()->addEventListener('keydown', fn($event) => $app->key($event));
U::document()->querySelector('#restart')->addEventListener('click', fn() => $app->newGame());
$app->newGame();
