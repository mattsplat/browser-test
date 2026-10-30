<?php
// Particles under gravity, bouncing off the edges, drawn every frame. Click to burst more.

use Unscripted as U;

const WIDTH = 640;
const HEIGHT = 400;
const GRAVITY = 900.0;   // pixels per second per second
const MAX_PARTICLES = 2000;
const COLORS = ['#FF6B6B', '#FFD93D', '#6BCB77', '#4D96FF', '#C77DFF', '#FF9F1C'];

final class Particle {
    public function __construct(
        public float $x,
        public float $y,
        public float $vx,
        public float $vy,
        public float $radius,
        public string $color,
    ) {}
}

final class Sky {
    /** @var Particle[] */
    public array $particles = [];
    private float $last;
    private int $frames = 0;
    private float $fpsSince;
    private int $fps = 0;

    public function __construct(private object $ctx, private object $stats) {
        $this->last = $this->fpsSince = U::performance()->now();
    }

    public function burst(float $x, float $y, int $count): void {
        for ($i = 0; $i < $count && count($this->particles) < MAX_PARTICLES; $i++) {
            $angle = mt_rand() / mt_getrandmax() * 2 * M_PI;
            $speed = 80 + mt_rand(0, 420);
            $this->particles[] = new Particle(
                $x, $y,
                cos($angle) * $speed, sin($angle) * $speed - 250,
                mt_rand(20, 50) / 10,
                COLORS[array_rand(COLORS)],
            );
        }
    }

    public function frame(): void {
        $now = U::performance()->now();
        $dt = min(($now - $this->last) / 1000, 0.05);
        $this->last = $now;
        $this->move($dt);
        $this->draw();
        $this->frames++;
        if ($now - $this->fpsSince >= 1000) {
            $this->fps = $this->frames;
            $this->frames = 0;
            $this->fpsSince = $now;
        }
        $this->stats->textContent = sprintf('%d particles · %d fps', count($this->particles), $this->fps);
    }

    private function move(float $dt): void {
        foreach ($this->particles as $p) {
            $p->vy += GRAVITY * $dt;
            $p->x += $p->vx * $dt;
            $p->y += $p->vy * $dt;
            if ($p->x < $p->radius || $p->x > WIDTH - $p->radius) {
                $p->vx *= -0.8;
                $p->x = max($p->radius, min(WIDTH - $p->radius, $p->x));
            }
            if ($p->y > HEIGHT - $p->radius) {
                $p->vy *= -0.7;
                $p->vx *= 0.98;
                $p->y = HEIGHT - $p->radius;
            }
        }
    }

    private function draw(): void {
        $ctx = $this->ctx;
        $ctx->fillStyle = '#0B1020';
        $ctx->fillRect(0, 0, WIDTH, HEIGHT);
        foreach ($this->particles as $p) {
            $ctx->fillStyle = $p->color;
            $ctx->beginPath();
            $ctx->arc($p->x, $p->y, $p->radius, 0, 2 * M_PI);
            $ctx->fill();
        }
    }
}

$canvas = U::document()->querySelector('#sky');
$sky = new Sky($canvas->getContext('2d'), U::document()->querySelector('#stats'));
$sky->burst(WIDTH / 2, HEIGHT / 3, 300);

$canvas->addEventListener('click', function ($event) use ($canvas, $sky) {
    $box = $canvas->getBoundingClientRect();
    $sky->burst($event->clientX - $box->left, $event->clientY - $box->top, 150);
});
U::document()->querySelector('#more')->addEventListener('click', fn() => $sky->burst(mt_rand(80, WIDTH - 80), 60, 200));
U::document()->querySelector('#clear')->addEventListener('click', function () use ($sky) {
    $sky->particles = [];
});

$tick = function () use ($sky, &$tick) {
    $sky->frame();
    U::requestAnimationFrame($tick);
};
U::requestAnimationFrame($tick);
