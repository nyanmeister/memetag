#!/usr/bin/env node
// Isolated Chromium regression checks: real fetches and real user-activation
// expiry, with native Android clipboard/share handoff replaced by observable stubs.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdtemp, readFile, rm} from 'node:fs/promises';
import {createServer} from 'node:http';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {once} from 'node:events';

const root = new URL('../', import.meta.url);
if (process.argv.includes('--version')) {
    const manifest = await readFile(new URL('Cargo.toml', root), 'utf8');
    console.log('memetag-web-check ' + manifest.match(/^version = "([^"]+)"/m)[1]);
    process.exit(0);
}
const baseline = process.argv.includes('--baseline');
const source = baseline ? await readFile(process.env.MEMETAG_WEB_BASELINE, 'utf8')
    : '<script>' + await readFile(new URL('src/web_actions.js', root), 'utf8') + '</script>';
const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=', 'base64');
const profile = await mkdtemp(join(tmpdir(), 'memetag-web-'));
let browser, client;
const counts = new Map(), sockets = new Set();
const server = createServer((req, res) => {
    const url = new URL(req.url, 'http://localhost');
    if (url.pathname === '/log') { res.writeHead(204); res.end(); return; }
    if (['/png', '/file'].includes(url.pathname)) {
        const key = url.searchParams.get('f');
        const count = (counts.get(key) || 0) + 1;
        counts.set(key, count);
        if (key === 'unavailable' || (key === 'retry' && count < 3)) {
            res.writeHead(503, {'Content-Type': 'text/plain'}); res.end('temporarily unavailable'); return;
        }
        if (key === 'missing') { res.writeHead(404); res.end('not found'); return; }
        if (key === 'hang') return;
        res.writeHead(200, {'Content-Type': 'image/png', 'Content-Length': png.length});
        if (key === 'truncated' && count === 1) {
            res.write(png.subarray(0, 10)); setTimeout(() => res.destroy(), 100); return;
        }
        if (key.startsWith('slow')) {
            res.write(png.subarray(0, 10));
            const timer = setTimeout(() => res.end(png.subarray(10)), 6500);
            res.on('close', () => clearTimeout(timer));
        } else res.end(png);
        return;
    }
    const key = url.searchParams.get('key') || 'fast';
    // The production script itself is exercised; test markup supplies its normal
    // anchors/toast. Native stubs check Chromium's ACTUAL activation state.
    res.writeHead(200, {'Content-Type': 'text/html'});
    res.end(`<!doctype html><html><head><style>
    a,button{display:inline-block;padding:14px;margin:8px}[hidden]{display:none}
    </style></head><body>
    <a class=s data-f="${key}" href="/share">Copy</a>
    <a class=sh data-f="${key}" data-n="fixture.png" href="/share">Share</a>
    <a class=o href="/file?f=${key}">Open</a><div id=toast hidden></div>
    <script>
    window.calls=[];window.denied=false;window.oldClipboard=false;
    window.errors=[];addEventListener('unhandledrejection',e=>errors.push(String(e.reason)));
    const Item=window.ClipboardItem;
    window.ClipboardItem=class {constructor(data){
        if(window.oldClipboard&&data['image/png'] instanceof Promise)throw new TypeError('old browser');
        return new Item(data);
    }};
    Object.defineProperty(navigator,'clipboard',{value:{write:async function(items){
        const active=navigator.userActivation.isActive;calls.push({kind:'copy',active});
        if(!active||window.denied)throw new DOMException('permission denied','NotAllowedError');
        const blob=await items[0].getType('image/png');calls[calls.length-1].size=blob.size;
    }}});
    navigator.canShare=()=>true;
    navigator.share=async function(data){
        const active=navigator.userActivation.isActive;calls.push({kind:'share',active,size:data.files[0].size});
        if(!active)throw new DOMException('expired user activation','NotAllowedError');
    };
    if(${JSON.stringify(key === 'hang')}) {
        const original=window.setTimeout;
        window.setTimeout=(fn,ms,...args)=>original(fn,ms>=20000?150:ms,...args);
    }
    </script>${source}</body></html>`);
});
server.on('connection', socket => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); });

class CDP {
    constructor(socket) {
        this.socket = socket; this.id = 0; this.pending = new Map();
        socket.addEventListener('message', e => {
            const m = JSON.parse(e.data), p = this.pending.get(m.id);
            if (p) { this.pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); }
        });
    }
    send(method, params = {}) {
        return new Promise((resolve, reject) => {
            const id = ++this.id; this.pending.set(id, {resolve, reject});
            this.socket.send(JSON.stringify({id, method, params}));
        });
    }
    async eval(expression) {
        const r = await this.send('Runtime.evaluate', {expression, returnByValue: true, awaitPromise: true});
        if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + ': ' + r.exceptionDetails.exception?.description);
        return r.result.value;
    }
    async click(selector) {
        const rect = await this.eval(`(()=>{const r=document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
        await this.send('Input.dispatchMouseEvent', {type: 'mousePressed', ...rect, button: 'left', clickCount: 1});
        await this.send('Input.dispatchMouseEvent', {type: 'mouseReleased', ...rect, button: 'left', clickCount: 1});
    }
}
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(expression, timeout = 11000) {
    const start = Date.now();
    while (Date.now() - start < timeout) { if (await client.eval(expression)) return; await sleep(50); }
    throw new Error('Timed out: ' + expression + '\n' + await client.eval('document.body.innerText'));
}
async function page(key) {
    await client.send('Page.navigate', {url: `http://127.0.0.1:${server.address().port}/?key=${key}`});
    await until(`document.readyState === 'complete' && new URL(location.href).searchParams.get('key') === ${JSON.stringify(key)} && !!document.querySelector('a.s') && typeof window.calls !== 'undefined'`);
}
function passed(name) { console.log('PASS ' + name); }
try {
    server.listen(0, '127.0.0.1'); await once(server, 'listening');
    browser = spawn(process.env.MEMETAG_TEST_BROWSER || 'chromium', [
        '--headless', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
        '--disable-background-networking', '--disable-component-update', '--disable-sync',
        '--remote-debugging-port=0', '--user-data-dir=' + profile, 'about:blank',
    ], {stdio: ['ignore', 'ignore', 'pipe'], detached: true});
    const endpoint = await new Promise((resolve, reject) => {
        let log = ''; const timer = setTimeout(() => reject(new Error('Chromium did not start: ' + log)), 15000);
        browser.on('error', reject);
        browser.stderr.on('data', chunk => {
            log = (log + chunk).slice(-4000); const match = log.match(/DevTools listening on (ws:\/\/[^\s]+)/);
            if (match) { clearTimeout(timer); resolve(match[1]); }
        });
    });
    const endpointURL = new URL(endpoint);
    const targets = await (await fetch(`http://${endpointURL.host}/json/list`)).json();
    const socket = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl);
    await new Promise((resolve, reject) => { socket.addEventListener('open', resolve); socket.addEventListener('error', reject); });
    client = new CDP(socket);
    await client.send('Page.enable'); await client.send('Network.enable');
    await client.send('Network.emulateNetworkConditions', {
        offline: false, latency: 200, downloadThroughput: 8192, uploadThroughput: 8192,
    });
    await page('slow-copy'); await client.click('a.s');
    if (baseline) {
        await until('calls.length && !calls[0].active');
        assert.equal(await client.eval('calls[0].active'), false);
        passed('baseline reproduces Copy after expired activation on a slow transfer');
    } else {
        await until('calls.length && calls[0].size > 0');
        assert.deepEqual(await client.eval('calls.map(c=>[c.kind,c.active,c.size])'), [['copy', true, png.length]]);
        passed('slow Copy starts during the tap and awaits complete image bytes');
        await page('slow-share'); await client.click('a.sh');
        await until('!!document.querySelector("#transfer button:not([hidden])") && document.body.innerText.includes("Share now")');
        assert.equal(await client.eval('calls.length'), 0);
        await client.click('#transfer button'); await until('calls.length > 0');
        assert.equal(await client.eval('calls[0].active'), true);
        assert.equal(counts.get('slow-share'), 1);
        passed('slow Share waits for a fresh tap without redownloading');
        await page('retry'); await client.click('a.s');
        await until('calls.length && calls[0].size > 0'); assert.equal(counts.get('retry'), 3);
        assert.equal(await client.eval('calls.length'), 1);
        passed('transient server errors retry downloads, never clipboard actions');
        await page('truncated'); await client.click('a.s');
        await until('calls.length && calls[0].size > 0');
        assert.equal(counts.get('truncated'), 2); assert.equal(await client.eval('calls[0].size'), png.length);
        passed('truncated transfer is discarded and retried as a complete image');
        await page('missing'); await client.click('a.sh');
        await until('document.body.innerText.includes("Retry")');
        assert.equal(counts.get('missing'), 1); assert.equal(await client.eval('calls.length'), 0);
        passed('missing file produces a retry control without automatic request loops');
        await page('denied'); await client.eval('window.denied=true'); await client.click('a.s');
        await until('document.body.innerText.includes("Copy now")');
        assert.equal(await client.eval('calls.length'), 1);
        await client.eval('window.denied=false'); await client.click('#transfer button');
        await until('calls.length===2 && calls[1].size>0');
        assert.equal(counts.get('denied'), 1);
        passed('denied permission retains bytes and waits for an explicit new tap');
        await page('slow-old'); await client.eval('window.oldClipboard=true'); await client.click('a.s');
        await until('document.body.innerText.includes("Copy now")');
        assert.equal(await client.eval('calls.length'), 0);
        await client.click('#transfer button'); await until('calls.length && calls[0].size>0');
        assert.equal(await client.eval('calls[0].active'), true);
        passed('older ClipboardItem uses a fresh tap after slow downloads');
        await page('slow-cancel'); await client.click('a.s'); await sleep(300);
        await client.click('#transfer button:last-child'); await sleep(7000);
        assert.equal(await client.eval('calls.some(c=>c.size>0)'), false);
        assert.equal(await client.eval('document.querySelector("#transfer").hidden'), true);
        assert.deepEqual(await client.eval('errors'), []);
        passed('cancellation prevents a late clipboard write and hides transfer controls');
        await page('hang'); await client.click('a.sh');
        await until('document.body.innerText.includes("timed out")');
        assert.equal(counts.get('hang'), 3); assert.equal(await client.eval('calls.length'), 0);
        passed('stalled transfers time out with bounded retries and recovery controls');
        await page('open'); await client.click('a.o'); await until('location.protocol === "blob:"');
        assert.equal(counts.get('open'), 1);
        passed('Open downloads a complete original before same-tab viewing');
    }
} finally {
    client?.socket.close();
    if (browser?.pid) {
        try { process.kill(-browser.pid, 'SIGTERM'); } catch {}
        await Promise.race([once(browser, 'exit'), sleep(2000)]);
        try { process.kill(-browser.pid, 'SIGKILL'); } catch {}
    }
    for (const socket of sockets) socket.destroy();
    server.close();
    await rm(profile, {recursive: true, force: true});
}
