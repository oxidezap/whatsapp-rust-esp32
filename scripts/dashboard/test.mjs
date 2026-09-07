import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { chromium } from 'playwright';
import jsQR from 'jsqr';

const root = new URL('../../', import.meta.url);
const source = await readFile(new URL('src/admin.rs', root), 'utf8');
const html = source.match(/const DASHBOARD_HTML: &str = r#"([\s\S]*?)"#;/)[1];
const payload = '2@' + 'aB09+/'.repeat(24) + ',' + ['b', 'c', 'd'].map(c => c.repeat(43) + '=').join(',');
let qr = payload;
const server = createServer(async (req, res) => {
  if (req.url === '/dashboard') {
    res.setHeader('Content-Type', 'text/html');
    res.end(html);
  } else if (req.url === '/qrcode.min.js') {
    try {
      res.setHeader('Content-Type', 'application/javascript');
      res.end(await readFile(new URL('src/assets/qrcode.min.js', root)));
    } catch {
      res.writeHead(404).end();
    }
  } else {
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(req.url === '/device'
      ? { qr_code: qr, connected: !qr, pair_code: { state: 'idle' } }
      : req.url === '/messages' ? { count: 0, messages: [] }
      : { heap_free: 65536, sessions: 0, identities: 0, prekeys: 0, uptime_s: 10,
        heap_internal_free: 65536, internal_8bit_min_free: 32768,
        internal_8bit_largest_block: 16384, psram_free: 4194304, psram_largest_block: 2097152 }));
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
let browser;
try {
  browser = await chromium.launch({ headless: true });
  for (const width of [1280, 320, 390]) {
    qr = payload;
    const context = await browser.newContext({
      viewport: { width, height: 900 },
      isMobile: width < 600, hasTouch: width < 600, deviceScaleFactor: width < 600 ? 2 : 1,
    });
    const page = await context.newPage();
    const external = [];
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await context.addInitScript(() => { window.setInterval = () => 0; });
    await context.route('**/*', async route => {
      if (new URL(route.request().url()).origin !== origin) {
        external.push(route.request().url());
        return route.abort();
      }
      return route.continue();
    });
    await page.goto(`${origin}/dashboard`);
    await page.waitForFunction(() => document.querySelector('#status').textContent !== 'Loading...');
    async function decode(expected) {
      const image = await page.locator('#qr-canvas canvas').evaluate(canvas => ({
        width: canvas.width, height: canvas.height,
        data: Array.from(canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data),
      }));
      assert.equal(jsQR(new Uint8ClampedArray(image.data), image.width, image.height)?.data, expected);
    }
    await decode(payload);
    assert.equal(await page.locator('#status').textContent(), 'Waiting for QR scan');
    const overflow = await page.evaluate(() => [...document.querySelectorAll('body *')]
      .filter(el => el.getBoundingClientRect().right > innerWidth)
      .map(el => ({ tag: el.tagName, id: el.id, text: el.textContent.slice(0, 60), right: el.getBoundingClientRect().right })));
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), JSON.stringify(overflow));
    const box = await page.locator('canvas').boundingBox();
    assert.ok(box.x >= 0 && box.x + box.width <= width);
    const screenshot = await page.locator('canvas').screenshot();
    const displayed = await page.evaluate(async base64 => {
      const image = new Image();
      image.src = 'data:image/png;base64,' + base64;
      await image.decode();
      const canvas = document.createElement('canvas');
      canvas.width = image.width;
      canvas.height = image.height;
      const ctx = canvas.getContext('2d');
      ctx.drawImage(image, 0, 0);
      return { width: canvas.width, height: canvas.height,
        data: Array.from(ctx.getImageData(0, 0, canvas.width, canvas.height).data) };
    }, screenshot.toString('base64'));
    assert.equal(jsQR(new Uint8ClampedArray(displayed.data), displayed.width, displayed.height)?.data, payload);
    await page.evaluate(() => { window.firstCanvas = document.querySelector('canvas'); });
    await page.evaluate(() => refresh());
    assert.ok(await page.evaluate(() => window.firstCanvas === document.querySelector('canvas')));

    qr = payload.replace('2@', '3@');
    await page.evaluate(() => refresh());
    await decode(qr);
    await page.evaluate(() => {
      window.originalToCanvas = QRCode.toCanvas;
      QRCode.toCanvas = (canvas, text, options, callback) => callback(new Error('test failure'));
    });
    qr = payload;
    await page.evaluate(() => refresh());
    assert.equal(await page.locator('canvas').count(), 0);
    assert.match(await page.locator('#qr-canvas').textContent(), /Could not render/);
    assert.equal(await page.locator('#status').textContent(), 'Waiting for QR scan');
    assert.match(await page.locator('#heap').textContent(), /64/);
    await page.evaluate(() => { QRCode.toCanvas = window.originalToCanvas; });
    await page.evaluate(() => refresh());
    await decode(qr);

    qr = null;
    await page.evaluate(() => refresh());
    assert.ok(await page.locator('#qr-section').evaluate(el => el.classList.contains('hidden')));
    assert.equal(await page.locator('canvas').count(), 0);
    qr = payload;
    await page.evaluate(() => refresh());
    await decode(qr);

    await context.route(`${origin}/qrcode.min.js`, route => route.abort());
    await page.reload();
    await page.waitForFunction(() => document.querySelector('#qr-canvas').textContent.includes('Could not render'));
    assert.equal(await page.locator('#status').textContent(), 'Waiting for QR scan');
    assert.match(await page.locator('#heap').textContent(), /64/);
    await context.unroute(`${origin}/qrcode.min.js`);
    await page.reload();
    await decode(qr);
    assert.deepEqual(external, [], 'Dashboard must never contact another origin');
    assert.deepEqual(errors, []);
    console.log(`QR decode, rotation, retry, clearing, same-origin requests and layout passed at ${width}px.`);
    await context.close();
  }
} finally {
  await browser?.close();
  await new Promise(resolve => server.close(resolve));
}
