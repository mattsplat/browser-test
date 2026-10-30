<?php
// The browser's objects come from the Unscripted class; `use` gives it a short name.
use Unscripted as U;

$document = U::document();
$document->querySelector('#status')->textContent = 'PHP ' . PHP_VERSION . ' is running, as a pack in WebAssembly.';
$clicks = 0;

$document->querySelector('#increment')->addEventListener('click', function ($event) use ($document, &$clicks) {
    $clicks++;
    $document->querySelector('#count')->textContent = (string) $clicks;
    $item = $document->createElement('li');
    $item->textContent = "Click $clicks, handled in PHP ({$event->type})";
    $document->querySelector('#log')->appendChild($item);
    U::console()->log("click $clicks");
});
