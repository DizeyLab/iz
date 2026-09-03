// The profile photo's overlay: with a photo set, clicking the avatar on the
// settings profile section opens the picture in the file viewer's chrome,
// and the picker moves to the Change button. `tests/http.rs` can assert the
// served markup; only a browser shows the client-built overlay opening, the
// three ways it closes (Escape, the X, the scrim), and the relocated input
// still autosubmitting — the class where the upload quietly did nothing.
//
// Wants the server run.sh leaves behind: the workspace already claimed
// (soft-nav claims it), so this signs in — but it claims too, so the
// script runs standalone as well:
//
//     node crates/iz-web/tests/browser/photo.mjs http://127.0.0.1:7791
//
// Playwright lives outside the repo, exactly as soft-nav.mjs says.
const { chromium } = await import(process.env.IZ_PLAYWRIGHT || 'playwright');
const zlib = await import('node:zlib');
const fs = await import('node:fs');
const os = await import('node:os');
const path = await import('node:path');

const base = process.argv[2] || 'http://127.0.0.1:7791';
const shots = process.env.SHOT_DIR || '.';
const failures = [];
const note = (line) => console.log(line);

// A real image, generated here so the check carries no fixture: the upload
// sniffs bytes, and the overlay assertion reads the served naturalWidth.
const W = 240;
const H = 160;
function makePng() {
    const table = [];
    for (let n = 0; n < 256; n++) {
        let c = n;
        for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
        table[n] = c >>> 0;
    }
    const crc32 = (buf) => {
        let c = 0xffffffff;
        for (const b of buf) c = table[(c ^ b) & 0xff] ^ (c >>> 8);
        return (c ^ 0xffffffff) >>> 0;
    };
    const chunk = (tag, data) => {
        const t = Buffer.from(tag, 'ascii');
        const len = Buffer.alloc(4);
        len.writeUInt32BE(data.length);
        const crc = Buffer.alloc(4);
        crc.writeUInt32BE(crc32(Buffer.concat([t, data])));
        return Buffer.concat([len, t, data, crc]);
    };
    const ihdr = Buffer.alloc(13);
    ihdr.writeUInt32BE(W, 0);
    ihdr.writeUInt32BE(H, 4);
    ihdr[8] = 8; // bit depth
    ihdr[9] = 2; // truecolor
    const rows = Buffer.alloc(H * (1 + W * 3));
    let o = 0;
    for (let y = 0; y < H; y++) {
        rows[o++] = 0;
        for (let x = 0; x < W; x++) {
            rows[o++] = Math.floor((x * 255) / (W - 1));
            rows[o++] = Math.floor((y * 255) / (H - 1));
            rows[o++] = 140;
        }
    }
    return Buffer.concat([
        Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
        chunk('IHDR', ihdr),
        chunk('IDAT', zlib.deflateSync(rows)),
        chunk('IEND', Buffer.alloc(0)),
    ]);
}
const photoPath = path.join(os.tmpdir(), 'iz-photo-check.png');
fs.writeFileSync(photoPath, makePng());

const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push('PE ' + e.message));
page.on('console', (m) => {
    if (m.type() === 'error') errors.push('CON ' + m.text());
});

// Claimed by soft-nav when run from run.sh, unclaimed standalone — either
// form signs the same admin in.
await page.goto(base + '/', { waitUntil: 'networkidle' });
if (await page.locator('input[name="display_name"]').count()) {
    await page.fill('input[name="display_name"]', 'Ada Lovelace');
    await page.fill('input[name="email"]', 'ada@iz.sh');
    await page.fill('input[name="password"]', 'correct horse battery staple');
    await Promise.all([page.waitForLoadState('networkidle'), page.click('button[type="submit"]')]);
} else {
    await page.fill('input[name="email"]', 'ada@iz.sh');
    await page.fill('input[name="password"]', 'correct horse battery staple');
    await Promise.all([page.waitForLoadState('networkidle'), page.click('button[type="submit"]')]);
}

await page.goto(base + '/settings', { waitUntil: 'networkidle' });
if (!(await page.locator('.identity-row').count())) {
    failures.push(`settings profile section never rendered at ${page.url()}`);
}

// No photo yet: the label is the picker. Setting a file on its input must
// autosubmit and swap in the viewer trigger — the relocation regression is
// exactly an upload that silently does nothing.
if (!failures.length) {
    await page.setInputFiles('input.file-upload-input', photoPath);
    const swapped = await page
        .waitForFunction(() => document.querySelector('button.avatar-view'), { timeout: 5000 })
        .then(() => true)
        .catch(() => false);
    if (!swapped) failures.push('the upload never swapped in the viewer trigger');
}

// The trigger opens the overlay; the picture must be the served photo, not
// a placeholder. Screenshot stands as the alignment check.
async function openViewer(label) {
    await page.click('.avatar-view');
    const open = await page
        .waitForFunction(
            () => {
                const m = document.querySelector('.viewer-scrim .viewer-media');
                return m && m.complete && m.naturalWidth > 0;
            },
            { timeout: 5000 },
        )
        .then(() => true)
        .catch(() => false);
    if (!open) {
        failures.push(`${label}: the overlay never opened with the photo`);
        return false;
    }
    const src = await page.locator('.viewer-scrim .viewer-media').getAttribute('src');
    if (!src || !src.includes('/photo/')) failures.push(`${label}: overlay image src is '${src}'`);
    const width = await page.evaluate(() => document.querySelector('.viewer-scrim .viewer-media').naturalWidth);
    if (width !== W) failures.push(`${label}: overlay serves naturalWidth ${width}, expected ${W}`);
    return true;
}
async function viewerGone(label) {
    const gone = await page
        .waitForFunction(() => !document.querySelector('.viewer-scrim'), { timeout: 5000 })
        .then(() => true)
        .catch(() => false);
    if (!gone) failures.push(`${label}: the overlay never closed`);
}

if (!failures.length) {
    if (await openViewer('escape')) {
        await page.screenshot({ path: `${shots}/photo-viewer.png` });
        await page.keyboard.press('Escape');
        await viewerGone('escape');
    }
    if (await openViewer('close-anchor')) {
        await page.click('.viewer-close');
        await viewerGone('close-anchor');
    }
    if (await openViewer('scrim')) {
        await page.mouse.click(4, 4);
        await viewerGone('scrim');
    }
}

// Change owns the picker now: the click must surface the chooser, and a
// chosen file must save through the same autosubmit the label used.
if (!failures.length) {
    if (!(await page.locator('[data-avatar-change]').count())) {
        failures.push('no Change button next to the photo');
    } else {
        const [chooser] = await Promise.all([
            page.waitForEvent('filechooser', { timeout: 5000 }).catch(() => null),
            page.click('[data-avatar-change]'),
        ]);
        if (!chooser) {
            failures.push('the Change click never opened the picker');
        } else {
            await chooser.setFiles(photoPath);
            const saved = await page
                .waitForFunction(() => document.querySelector('.identity-row .field-note'), { timeout: 5000 })
                .then(() => true)
                .catch(() => false);
            if (!saved) failures.push('the Change upload never confirmed the save');
            if (await page.locator('.identity-row .field-error').count()) {
                failures.push(`the Change upload drew a refusal: ${await page.locator('.identity-row .field-error').textContent()}`);
            }
        }
    }
}

// The everyday path: settings arrives by soft navigation, and by the live
// refresh's morph after it — not by document load. The swap re-creates the
// inline scripts and the morph preserves them; either going wrong passes
// every check above and dies for anyone who simply navigated.
if (!failures.length) {
    await page.goto(base + '/', { waitUntil: 'networkidle' });
    await page.click('a[href="/settings"]');
    await page
        .waitForFunction(() => location.pathname === '/settings', { timeout: 5000 })
        .catch(() => failures.push('the Settings nav click never soft-navigated'));
    if (!failures.length && (await openViewer('soft-nav'))) {
        await page.keyboard.press('Escape');
        await viewerGone('soft-nav');
    }
}
if (!failures.length) {
    await page.evaluate(() => window.__izRefresh());
    // Sleeping is the measurement: the refresh lands without an event.
    await page.waitForTimeout(800);
    if (await openViewer('post-refresh')) {
        await page.keyboard.press('Escape');
        await viewerGone('post-refresh');
    }
}

await browser.close();

note(`page errors ${errors.length ? errors.join(' | ') : 'none'}`);
if (errors.length) failures.push(`page errors: ${errors.join(' | ')}`);
if (failures.length) {
    for (const f of failures) note('FAIL ' + f);
    process.exit(1);
}
note('PASS the photo overlay opens on the avatar click, closes all three ways, and Change saves through the picker');
