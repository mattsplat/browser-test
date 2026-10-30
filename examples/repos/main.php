<?php
// A user's or organisation's public repositories from the GitHub API, as a sortable table.

use Unscripted as U;

final class Repos {
    /** @var array<int, array{name: string, url: string, description: string, language: string, stars: int, forks: int, updated: string}> */
    private array $repos = [];
    private string $sortKey = 'stars';
    private bool $descending = true;

    public function load(string $who): void {
        $who = trim($who);
        if (!preg_match('/^[A-Za-z0-9-]{1,39}$/', $who)) {
            $this->status('That isn\'t a GitHub user or organisation name.');
            return;
        }
        $this->status("Loading $who's repositories…");
        $url = 'https://api.github.com/users/' . rawurlencode($who) . '/repos?per_page=100&sort=updated';
        U::fetch($url)->then(
            function ($response) use ($who) {
                if (!$response->ok) {
                    $this->status("GitHub said {$response->status} {$response->statusText} for $who.");
                    return;
                }
                $response->json()->then(fn($data) => $this->show($who, $data));
            },
            fn($error) => $this->status("Couldn't load: $error"),
        );
    }

    /** Keep the fields the table shows, as PHP arrays (the host's data is read once, here). */
    private function show(string $who, object $data): void {
        $this->repos = [];
        for ($i = 0; $i < $data->length; $i++) {
            $repo = $data[$i];
            $this->repos[] = [
                'name' => $repo->name,
                'url' => $repo->html_url,
                'description' => (string) $repo->description,
                'language' => (string) $repo->language,
                'stars' => (int) $repo->stargazers_count,
                'forks' => (int) $repo->forks_count,
                'updated' => substr($repo->updated_at, 0, 10),
            ];
        }
        $this->status(sprintf('%d public repositories for %s.', count($this->repos), $who));
        $this->render();
    }

    public function sortBy(string $key): void {
        // Clicking the same heading again flips the order.
        $this->descending = $key === $this->sortKey ? !$this->descending : in_array($key, ['stars', 'forks', 'updated'], true);
        $this->sortKey = $key;
        $this->render();
    }

    private function render(): void {
        $key = $this->sortKey;
        usort($this->repos, function ($a, $b) use ($key) {
            $order = is_int($a[$key]) ? $a[$key] <=> $b[$key] : strcasecmp($a[$key], $b[$key]);
            return $this->descending ? -$order : $order;
        });
        $document = U::document();
        foreach ($document->querySelectorAll('th') as $heading) {
            $name = rtrim($heading->textContent, ' ▲▼');
            $heading->textContent = $heading->dataset['key'] === $key ? $name . ($this->descending ? ' ▼' : ' ▲') : $name;
        }
        $rows = $document->querySelector('#rows');
        $rows->textContent = '';
        foreach ($this->repos as $repo) {
            $row = $document->createElement('tr');
            $name = $document->createElement('td');
            $link = $document->createElement('a');
            $link->setAttribute('href', $repo['url']);
            $link->textContent = $repo['name'];
            $name->appendChild($link);
            if ($repo['description'] !== '') {
                $description = $document->createElement('div');
                $description->className = 'description';
                $description->textContent = $repo['description'];
                $name->appendChild($description);
            }
            $row->appendChild($name);
            foreach ([['language', ''], ['stars', 'num'], ['forks', 'num'], ['updated', '']] as [$field, $class]) {
                $cell = $document->createElement('td');
                $cell->textContent = (string) $repo[$field];
                if ($class !== '') {
                    $cell->className = $class;
                }
                $row->appendChild($cell);
            }
            $rows->appendChild($row);
        }
    }

    private function status(string $text): void {
        U::document()->querySelector('#status')->textContent = $text;
    }
}

$repos = new Repos();
U::document()->querySelector('#search')->addEventListener('submit', function ($event) use ($repos) {
    $event->preventDefault();
    $repos->load(U::document()->querySelector('#who')->value);
});
foreach (U::document()->querySelectorAll('th') as $heading) {
    $heading->addEventListener('click', fn() => $repos->sortBy($heading->dataset['key']));
}
$repos->load(U::document()->querySelector('#who')->value);
