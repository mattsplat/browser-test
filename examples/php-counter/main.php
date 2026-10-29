<?php
$document->querySelector('#status')->textContent = 'PHP ' . PHP_VERSION . ' is running, as a pack in WebAssembly.';
$clicks = 0;

// $document is a superglobal here, so the closure needs no `use`.
$document->querySelector('#increment')->addEventListener('click', function ($event) {
    global $clicks;

    $clicks++;
    $document->querySelector('#count')->textContent = (string) $clicks;

    $item = $document->createElement('li');
    $item->textContent = "Click $clicks, handled in PHP ({$event->type})";

    $document->querySelector('#log')->appendChild($item);
    $console->log("click $clicks");
    $console->log(var_export($document, true));
});
