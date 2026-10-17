# Hosting GridLogic for the web

`gridcraft-web-<version>.zip` (from the GitHub release, or `packaging/web/package.sh`) holds a
static site in `gridcraft-web-<version>/`:

| File | What it is |
|---|---|
| `index.html` | The page. It loads everything through relative URLs. |
| `gridcraft-web-<hash>.js` | wasm-bindgen glue (generated, ES module) |
| `gridcraft-web-<hash>_bg.wasm` | The app (compresses to roughly a third with gzip/Brotli) |
| `_headers`, `.htaccess` | Sample header rules for Netlify/Cloudflare Pages and Apache |

There is no server-side code. Upload the folder's contents anywhere that serves static files.

## Any path works

All URLs in `index.html` are relative (`public_url = "./"` in `apps/gridcraft-web/Trunk.toml`),
so the site works at a domain root (`https://example.com/`), under a prefix
(`https://example.com/tools/gridcraft/`) and from a CDN bucket. The asset names carry a content
hash, so they can be cached forever. Only `index.html` needs revalidation.

## Required server settings

- **MIME type:** serve `.wasm` as `application/wasm`. Browsers refuse to stream-compile it under
  any other type, and the app then loads slowly or not at all. Serve `.js` as `text/javascript`.
  Most hosts already do both. For nginx, check that `mime.types` has `application/wasm wasm;`.
- **Compression:** turn on gzip or Brotli for `.wasm`, `.js` and `.html`. That cuts the
  download to roughly a third. You can also precompress (`brotli -k *.wasm`) and let
  the server send `Content-Encoding: br`.
- **Caching:** `Cache-Control: public, max-age=31536000, immutable` on the hashed `.wasm` and
  `.js` files, and `no-cache` on `index.html`.
- **HTTPS:** WebGPU (and the clipboard) only work in a secure context, which means `https://`
  or `http://localhost`. Over plain HTTP elsewhere, the app falls back to WebGL2.
- **No special isolation headers:** GridLogic doesn't use `SharedArrayBuffer`, so it doesn't
  need `Cross-Origin-Opener-Policy` or `Cross-Origin-Embedder-Policy`. If your site already sends
  COEP `require-corp`, also send `Cross-Origin-Resource-Policy: same-origin` (or `cross-origin`
  when the files live on a CDN) on the app's files.

nginx example:

```nginx
location /gridcraft/ {
    types { application/wasm wasm; text/javascript js; text/html html; }
    gzip on;
    gzip_types application/wasm text/javascript text/html;
    location ~* \.(wasm|js)$ { add_header Cache-Control "public, max-age=31536000, immutable"; }
    location ~* index\.html$ { add_header Cache-Control "no-cache"; }
}
```

Local test: `python3 -m http.server 8765` inside the folder, then open http://localhost:8765/.

## Embedding in a page (iframe)

```html
<iframe
  src="https://example.com/gridcraft/"
  title="GridLogic spreadsheet"
  style="width: 100%; height: 720px; border: 0;"
  allow="fullscreen; clipboard-read; clipboard-write"
  allowfullscreen>
</iframe>
```

- The app fills the iframe and follows its size, so size the iframe and not the app.
- Keyboard shortcuts go to the iframe after the user clicks into it, as with any embedded app.
- **Cross-origin embeds** work. Preferences are kept in the iframe's `localStorage`. Browsers
  that partition or block third-party storage may forget them between visits, and the app
  then starts with defaults.
- **Sandboxed iframes** need at least
  `sandbox="allow-scripts allow-same-origin allow-downloads allow-popups"`. Without
  `allow-same-origin` there's no storage. Without `allow-downloads`, Save and Export (browser
  downloads) are blocked.
- Don't send `X-Frame-Options: DENY` or a `frame-ancestors` CSP that excludes the embedding page.

## Opening and saving through the page (`?host=parent`)

With `?host=parent` in the iframe `src`, GridLogic opens workbooks the embedding page sends it
and hands the page every file it saves instead of downloading it. This suits a page with its own
storage, such as a Nextcloud app. The page must be on the same origin as GridLogic: messages
from anywhere else are ignored. In this mode GridLogic starts with an empty workbook.

| Message | Direction | Fields |
|---|---|---|
| `gridcraft:ready` | to the page | `version`; `ack` (true with `&ack=1`); `collab` (true: this build can co-edit). Send the file to open after this. |
| `gridcraft:open` | from the page | `name`, `bytes` (`ArrayBuffer` or `Uint8Array`) |
| `gridcraft:save` | to the page | `name`, `bytes` (a transferred `ArrayBuffer`); `id` with `&ack=1`. Sent for Save, Save As (.xlsx) and Export as PDF. |
| `gridcraft:saved` | from the page (`&ack=1`) | `id`; optional `name` when the page stored the file under another name |
| `gridcraft:saveFailed` | from the page (`&ack=1`) | `id`, `error` (shown to the user) |
| `gridcraft:dirty` | to the page | `dirty`: whether there are unsaved changes |
| `gridcraft:failed` | to the page | `error`: the app couldn't start |

Other query flags: `&author=Name` signs new notes and comments.

**Save confirmation (`&ack=1`).** Without it, a workbook counts as saved as soon as GridLogic
hands it to the page, even if the page then fails to store it. With it, the workbook counts as
saved only when the page answers `gridcraft:saved` with the save's `id`. It is then saved as it
was when handed over, so edits made since stay unsaved. `gridcraft:saveFailed` leaves it unsaved
and shows the user the page's `error`. An older GridLogic ignores the flag; it answers
`gridcraft:ready` without `ack: true`.

### Co-editing

Several people can edit one workbook at once (`docs/co-editing.md`). GridLogic does all the
merging; the page connects it to a server that orders everyone's edits into one numbered log
(for Nextcloud, the `gridcraft-nextcloud` app's `/apps/gridcraft/collab/*` endpoints).

| Message | Direction | Fields |
|---|---|---|
| `gridcraft:collab:start` | from the page | `client` (this person's id from the server, 1 to 2³²−1), `seq`, optional `readOnly` (this person can view but not edit: edits are taken back with a message). Send right after `gridcraft:open`: the workbook just opened is the shared one as of log position `seq`. |
| `gridcraft:collab:entries` | from the page | `json`: the server's log entries in order, as JSON text: `[{seq, client, client_seq, base, op}]`, `op` exactly as the server received it. An entry whose op doesn't parse becomes a no-op in its place. |
| `gridcraft:collab:op` | to the page | `json`: this person's next edit for the server, `{client, client_seq, base, op}`. The next comes once this one is back in the log, so send them in order and retry a failed one (the server must recognise a repeated `client`/`client_seq`). |
| `gridcraft:collab:checkpoint` | from the page | `id`: asks for the shared workbook to save to the file. |
| `gridcraft:collab:checkpointData` | to the page | `id`, `seq`, `name`, `bytes` (.xlsx): the workbook as everyone has it at log position `seq`, without this person's unconfirmed edits. |
| `gridcraft:collab:selection` | to the page | `json`: this person's position, `{sheet, active, ranges}` (sheet id, cell, ranges), whenever it changes. |
| `gridcraft:collab:presence` | from the page | `json`: where the others are, `[{name, color: "#rrggbb", sheet, active, ranges}]`; drawn on the grid in their colour with their name. |
| `gridcraft:collab:stop` | from the page | Ends sharing. |
| `gridcraft:collab:error` | to the page | `error`: this copy can't follow the log (an entry missing or out of order). Rejoin: load the file again and start over. |

The server's side of the contract:
- Positions are gap-free, from 1.
- Joining returns the position the current file was saved at, plus every entry after it.
- A joiner that loaded the file before a newer save has to load it again.
- Entries older than the oldest `base` anyone may still send must be kept.

While sharing, Undo and Redo work on this person's own edits only, and never take back a later
change of someone else's. GridLogic handles co-editing messages as they arrive, even in a hidden
tab, which draws no frames.

```js
const frame = document.querySelector("iframe"); // src="/gridcraft/?host=parent&ack=1"
addEventListener("message", async (event) => {
  if (event.origin !== location.origin || event.source !== frame.contentWindow) return;
  const m = event.data;
  if (m.type === "gridcraft:ready") {
    const bytes = await (await fetch(fileUrl)).arrayBuffer();
    frame.contentWindow.postMessage({ type: "gridcraft:open", name: "Budget.xlsx", bytes }, location.origin, [bytes]);
  } else if (m.type === "gridcraft:save") {
    const r = await fetch(fileUrl, { method: "PUT", body: m.bytes });
    const reply = r.ok ? { type: "gridcraft:saved", id: m.id } : { type: "gridcraft:saveFailed", id: m.id, error: `${r.status} ${r.statusText}` };
    frame.contentWindow.postMessage(reply, location.origin);
  }
});
```

## Renderer selection

GridLogic's web build is the same egui app as the desktop one, compiled to wasm
(`apps/gridcraft-web`). It renders with the backend `apps/gridcraft-web` selects (WebGPU
where available, otherwise WebGL2). If the web app adds URL flags to force a backend (as the
sibling Craft apps do with `?webgl` / `?cpu`), document them here; they also work on the
iframe `src`.

A browser with neither WebGPU nor WebGL2 gets a message in place of the app.
