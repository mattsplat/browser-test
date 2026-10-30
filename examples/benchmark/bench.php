<?php
// The benchmark in PHP. bench.js, bench.py and bench.dart do the same work the same way, so the
// chart compares the languages, not the algorithms.

use Unscripted as U;

const LANG = 'php';
const LANGUAGES = [['js', 'JavaScript', '#D4A72C'], ['python', 'Python', '#3B82F6'], ['php', 'PHP', '#8B5CF6'], ['dart', 'Dart', '#14B8A6']];
const TASKS = [
    ['fib', 'fib(25), recursive'],
    ['sieve', 'Primes below 1,000,000'],
    ['strings', 'Join and split 20,000 strings'],
    ['dom', 'Create and remove 1,000 elements'],
    ['canvas', 'Draw 3,000 rectangles'],
];

function fib(int $n): int {
    return $n < 2 ? $n : fib($n - 1) + fib($n - 2);
}

// Each task returns a check value, the same in every language.
function run_task(string $key): int {
    $document = U::document();
    switch ($key) {
        case 'fib':
            return fib(25);
        case 'sieve':
            $n = 1000000;
            $composite = str_repeat("\0", $n + 1);
            $count = 0;
            for ($i = 2; $i <= $n; $i++) {
                if ($composite[$i] === "\0") {
                    $count++;
                    for ($j = $i * $i; $j <= $n; $j += $i) {
                        $composite[$j] = "\1";
                    }
                }
            }
            return $count;
        case 'strings':
            $parts = [];
            for ($i = 0; $i < 20000; $i++) {
                $parts[] = 'item ' . $i;
            }
            return count(explode(',', implode(',', $parts)));
        case 'dom':
            $list = $document->querySelector('#dom');
            for ($i = 0; $i < 1000; $i++) {
                $item = $document->createElement('li');
                $item->textContent = 'item ' . $i;
                $list->appendChild($item);
            }
            $count = $list->children->length;
            $list->textContent = '';
            return $count;
        case 'canvas':
            // A Park-Miller sequence, stepped with Schrage's method so no product passes 2^31 (PHP's integers are 32-bit here): every language draws the same rectangles in the same colors.
            $ctx = $document->querySelector('#scratch')->getContext('2d');
            $seed = 1;
            $next = function () use (&$seed) {
                $seed = 48271 * ($seed % 44488) - 3399 * intdiv($seed, 44488);
                if ($seed <= 0) {
                    $seed += 2147483647;
                }
                return $seed;
            };
            for ($i = 0; $i < 3000; $i++) {
                $x = $next() % 300;
                $y = $next() % 150;
                $c = $next();
                $ctx->fillStyle = sprintf('rgb(%d, %d, %d)', $c % 256, ($c >> 8) % 256, ($c >> 16) % 256);
                $ctx->fillRect($x, $y, 10, 10);
            }
            return $seed;
    }
    throw new Error("no task $key");
}

function status(string $text): void {
    U::document()->querySelector('#status')->textContent = $text;
}

function add_row(string $label, float $ms, int $check): void {
    $document = U::document();
    $row = $document->createElement('tr');
    foreach ([[$label, ''], [sprintf('%.1f', $ms), 'ms'], [(string) $check, '']] as [$text, $class]) {
        $cell = $document->createElement('td');
        $cell->textContent = $text;
        if ($class !== '') {
            $cell->className = $class;
        }
        $row->appendChild($cell);
    }
    $document->querySelector('#rows')->appendChild($row);
}

$results = [];

// Each task runs in its own timer callback, so the page repaints between them and no single
// callback comes near the 5 s limit.
function step(int $i): void {
    global $results;
    if ($i === count(TASKS)) {
        $pairs = array_map(fn($task) => sprintf('%s=%.2f', $task[0], $results[$task[0]]), TASKS);
        U::localStorage()->setItem('bench:' . LANG, implode(';', $pairs));
        status('Done. Run the other languages to compare them.');
        draw_chart();
        return;
    }
    [$key, $label] = TASKS[$i];
    status("Running: $label…");
    U::setTimeout(function () use ($i, $key, $label) {
        global $results;
        $start = U::performance()->now();
        $check = run_task($key);
        $ms = U::performance()->now() - $start;
        $results[$key] = $ms;
        add_row($label, $ms, $check);
        step($i + 1);
    }, 0);
}

/** Every language's saved results, as [lang => [task => ms]]. */
function saved_results(): array {
    $all = [];
    foreach (LANGUAGES as [$code]) {
        $saved = U::localStorage()->getItem('bench:' . $code);
        if (!$saved) {
            continue;
        }
        foreach (explode(';', $saved) as $pair) {
            [$key, $ms] = explode('=', $pair);
            $all[$code][$key] = (float) $ms;
        }
    }
    return $all;
}

/** One group of bars per task, one bar per language; the slowest fills the width. */
function draw_chart(): void {
    $ctx = U::document()->querySelector('#chart')->getContext('2d');
    $all = saved_results();
    $ctx->fillStyle = '#ffffff';
    $ctx->fillRect(0, 0, 640, 380);
    $ctx->font = '12px sans-serif';
    $y = 16;
    foreach (TASKS as [$key, $label]) {
        $ctx->fillStyle = '#222222';
        $ctx->fillText($label, 8, $y);
        $y += 6;
        $biggest = max([1e-9, ...array_map(fn($r) => $r[$key] ?? 0, array_values($all))]);
        foreach (LANGUAGES as [$code, $name, $color]) {
            if (!isset($all[$code])) {
                continue;
            }
            $ms = $all[$code][$key];
            $width = max(1, $ms / $biggest * 440);
            $ctx->fillStyle = $color;
            $ctx->fillRect(90, $y, $width, 11);
            $ctx->fillStyle = '#444444';
            $ctx->fillText($name, 8, $y + 10);
            $ctx->fillText(sprintf('%.1f ms', $ms), 96 + $width, $y + 10);
            $y += 13;
        }
        $y += 14;
    }
    if (!$all) {
        $ctx->fillStyle = '#888888';
        $ctx->fillText('No results yet: press Run.', 8, $y);
    }
}

U::document()->querySelector('#run')->addEventListener('click', function () {
    global $results;
    $results = [];
    U::document()->querySelector('#rows')->textContent = '';
    step(0);
});
U::document()->querySelector('#clear')->addEventListener('click', function () {
    foreach (LANGUAGES as [$code]) {
        U::localStorage()->removeItem('bench:' . $code);
    }
    U::document()->querySelector('#rows')->textContent = '';
    draw_chart();
});
status('PHP is ready.');
draw_chart();
