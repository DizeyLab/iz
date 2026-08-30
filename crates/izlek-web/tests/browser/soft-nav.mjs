// The one check `cargo test` cannot make: that the soft swap actually holds
// in a browser. `tests/http.rs` asserts the shape of the script İzlek serves;
// only a real engine runs it.
//
// Guards the URL-lag class. `swap()` (`src/layout.rs`) replaces the body,
// which re-executes the new page's inline scripts; `logs.rs`'s fit script
// reads `location.href` and reloads through it. While the address bar was
// rewritten after the swap rather than before, clicking Logs painted the
// page and then hard-navigated back to the board a few hundred milliseconds
// later — one press lost, every first visit.
//
// Wants a server on a throwaway database with the workspace unclaimed:
//
//     node crates/izlek-web/tests/browser/soft-nav.mjs http://127.0.0.1:7791
//
// Playwright lives outside the repo — İzlek has no node dependency and
// gains none here. Point PLAYWRIGHT_BROWSERS_PATH at the browser download
// and IZLEK_PLAYWRIGHT at the installed package (ESM resolves imports from
// this file's directory, not the working one, so a bare name will not do).
const { chromium } = await import(process.env.IZLEK_PLAYWRIGHT || 'playwright');

const base = process.argv[2] || 'http://127.0.0.1:7791';
const shots = process.env.SHOT_DIR || '.';
const failures = [];
const note = (line) => console.log(line);

const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push('PE ' + e.message));
page.on('console', (m) => {
    if (m.type() === 'error') errors.push('CON ' + m.text());
});
// Every full document load, soft-nav's whole point being that there is
// exactly one of them: the claim.
const loads = [];
page.on('load', () => loads.push(page.url()));

// The workspace is unclaimed, so the first page is the claim form and
// filling it signs the admin in.
await page.goto(base + '/', { waitUntil: 'networkidle' });
await page.fill('input[name="display_name"]', 'Ada Lovelace');
await page.fill('input[name="email"]', 'ada@izlek.sh');
await page.fill('input[name="password"]', 'correct horse battery staple');
await Promise.all([
    page.waitForLoadState('networkidle'),
    page.click('button[type="submit"]'),
]);
if (!/\/$/.test(new URL(page.url()).pathname)) {
    failures.push(`claiming the workspace landed on ${page.url()}`);
}
await page.screenshot({ path: `${shots}/board.png`, fullPage: true });

// Two visits, because they exercise different halves. The first runs the
// fit script for real: it sets its row cookie and reloads *itself*, which
// is by design — what must never happen is a load of any other URL, which
// is exactly what the URL-lag bug produced. The second visit finds the fit
// guard already set, so nothing reloads and the swap has to carry the page
// on its own.
async function visitLogs(label, { swapOnly }) {
    const before = loads.length;
    // Survives a swap (the document is never replaced) and dies with any
    // real navigation, which is the whole question being asked.
    await page.evaluate(() => {
        window.__softNavProbe = 'alive';
    });

    const logs = page.locator('a[href="/logs"]').first();
    if (!(await logs.count())) {
        failures.push(`${label}: no /logs link on the board`);
        return;
    }
    await logs.click();
    await page
        .waitForFunction(() => location.pathname === '/logs', { timeout: 5000 })
        .catch(() => failures.push(`${label}: the Logs click never reached /logs`));
    // The bounce took ~200ms; a second and a half is that window and then
    // some. Sleeping is the measurement — there is no event for "the page
    // did not go anywhere".
    await page.waitForTimeout(1500);

    const where = new URL(page.url()).pathname;
    if (where !== '/logs') {
        failures.push(
            `${label}: bounced off /logs to ${where} — the swap ran the new page's scripts under the old URL`,
        );
    }
    const strayed = loads
        .slice(before)
        .map((u) => new URL(u).pathname)
        .filter((p) => p !== '/logs');
    if (strayed.length) {
        failures.push(`${label}: loaded ${strayed.join(', ')} instead of staying on Logs`);
    }
    if (swapOnly) {
        if (loads.length > before) {
            failures.push(`${label}: reloaded ${loads.length - before} time(s); the fit guard was already set, so this had to be a swap`);
        }
        const probe = await page.evaluate(() => window.__softNavProbe || null);
        if (probe !== 'alive') {
            failures.push(`${label}: the document was replaced — Logs arrived by a real navigation, not a swap`);
        }
    }
    const body = await page.textContent('body');
    if (!body || body.trim().length < 40) {
        failures.push(`${label}: the logs page rendered blank`);
    }
    await page.screenshot({ path: `${shots}/logs-${label}.png`, fullPage: true });
}

// First press: the fit script is allowed to reload itself, nothing else.
await visitLogs('first', { swapOnly: false });

// Home again, then press Logs a second time — the press that used to be
// the only one that worked, and now has to be a pure swap.
await page.locator('a[href="/"]').first().click();
await page
    .waitForFunction(() => location.pathname === '/', { timeout: 5000 })
    .catch(() => failures.push('the board link never came home'));
await page.waitForTimeout(500);
await visitLogs('second', { swapOnly: true });

// Back has to come home the same way.
await page.goBack();
await page.waitForTimeout(800);
if (new URL(page.url()).pathname !== '/') {
    failures.push(`back from Logs landed on ${new URL(page.url()).pathname}`);
}

await browser.close();

note(`document loads ${loads.map((u) => new URL(u).pathname).join(' → ')} · page errors ${errors.length ? errors.join(' | ') : 'none'}`);
if (errors.length) failures.push(`page errors: ${errors.join(' | ')}`);
if (failures.length) {
    for (const f of failures) note('FAIL ' + f);
    process.exit(1);
}
note('PASS soft-nav holds: Logs stayed put on both presses, the second by swap alone, and back came home');
