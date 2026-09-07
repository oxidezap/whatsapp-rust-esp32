# Dashboard QR renderer

`qrcode.min.js` is generated from the unmodified `lib/browser.js` entry in
[`qrcode` 1.5.4](https://github.com/soldair/node-qrcode/tree/5d87b5f1a333ffb402dc0754a3f5f58f8fca2299).
The npm release does not contain `build/qrcode.min.js`. The generated IIFE
exposes the upstream `QRCode.toCanvas` API without a runtime module loader.

The bundle contains `qrcode` 1.5.4 by Ryan Day and `dijkstrajs` 1.0.3 by Wyatt
Baldwin, both MIT licensed. Their license texts are included at the top of the
JavaScript so firmware distributions carry them too. No QR algorithm or upstream
source has been modified. esbuild 0.28.2 bundles and minifies the browser entry.
The npm lockfile in `scripts/dashboard` records the source tarball URLs and
SHA-512 integrity hashes, including build and test dependencies.

The firmware embeds the file with `include_bytes!` and serves it directly from
flash at `/qrcode.min.js`. It does not allocate a copy of the bundle or encode QR
images on the ESP32. The asset contains no credentials. The dashboard fetches
pairing data from the existing token-protected `/device` fields and renders it
in the browser without external requests. The asset uses `Cache-Control: no-cache`
so a firmware update does not leave an old renderer in the browser cache.

## Regenerate and test

Normal firmware builds need neither Node.js nor npm. To regenerate the checked-in
asset after reviewing an upstream update, run from the repository root.

```sh
npm ci --ignore-scripts --prefix scripts/dashboard
npm run vendor --prefix scripts/dashboard
node scripts/dashboard/vendor.mjs --check
mkdir -p target/dashboard-tmp
TMPDIR="$PWD/target/dashboard-tmp" npm exec --prefix scripts/dashboard -- playwright install chromium
npm test --prefix scripts/dashboard
```

The browser test serves the actual dashboard HTML with synthetic API responses.
It blocks external requests, decodes the canvas with the independent `jsQR`
decoder, and checks QR rotation, render failure recovery, unchanged QR reuse,
clearing after pairing, script load failure, and desktop and mobile layouts.
It also decodes screenshots to check the displayed QR after CSS scaling. CI runs
the suite and checks that rebuilding the bundle produces the checked-in bytes.
The test does not pair an account or contact a physical board.
