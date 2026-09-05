// The avatar proxy: iz serves no photo of its own anymore — the settings
// profile row and the person page both wear `<img src="/avatar/{id}">`,
// proxied per request from im's photo for the caller's `sub`. `tests/http.rs`
// asserts the bytes and headers; only a browser shows the picture actually
// rendering (the hide-on-error script buries a bad proxy under the initials
// with zero errors) and surviving the live refresh's morph.
//
// Gone with SSO, deleted not commented: the upload page, the file input,
// the Change button, and the avatar overlay viewer — there is no im-side
// upload to drive, so none of that can open. What survives is the proxy:
// the fake im holds one fixed PNG at GET /photo/browser-ada, and both pages
// must render it.
//
// Wants the server run.sh leaves behind, plus the session cookie it mints:
// IZ_SESSION_COOKIE carries the sealed token the fake im knows — the same
// minted session soft-nav runs on, so the owner is already provisioned.
// Standalone runs need it too: there is no form left to sign in through:
//
//     node crates/iz-web/tests/browser/photo.mjs http://127.0.0.1:7791
//
// Playwright lives outside the repo, exactly as soft-nav.mjs says.
const { chromium } = await import(process.env.IZ_PLAYWRIGHT || 'playwright');

const base = process.argv[2] || 'http://127.0.0.1:7791';
const shots = process.env.SHOT_DIR || '.';
const failures = [];
const note = (line) => console.log(line);

// The fake im's fixed photo: a 1x1 PNG, so a rendered avatar reads
// naturalWidth 1 and anything else is the initials fallback showing.
const PHOTO_WIDTH = 1;

const session = process.env.IZ_SESSION_COOKIE;
if (!session) {
    note('FAIL IZ_SESSION_COOKIE is empty — run through run.sh');
    process.exit(1);
}

const browser = await chromium.launch();
const ctx = await browser.newContext();
await ctx.addCookies([{ name: 'iz_session', value: session, url: base }]);
const page = await ctx.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push('PE ' + e.message));
page.on('console', (m) => {
    if (m.type() === 'error') errors.push('CON ' + m.text());
});

await page.goto(base + '/settings', { waitUntil: 'networkidle' });
if (!(await page.locator('.identity-row').count())) {
    failures.push(`settings profile section never rendered at ${page.url()}`);
}

// The row wears the im identity the fake answered with — the browser-side
// proof the minted session provisioned the owner.
if (!failures.length) {
    const who = await page.evaluate(() => ({
        name: document.querySelector('.identity-row .identity-name')?.textContent?.trim() || '',
        address: document.querySelector('.identity-row .identity-address')?.textContent?.trim() || '',
    }));
    if (who.name !== 'Ada Lovelace') failures.push(`the profile row names '${who.name}'`);
    if (who.address !== 'ada@iz.sh') failures.push(`the profile row addresses '${who.address}'`);
}

// The avatar must be the proxied photo, and it must actually render: a
// broken proxy hides the img under the initials with no error to catch.
async function avatarRendered(label) {
    const img = page.locator('.identity-row img.avatar-photo, .person-head img.avatar-photo').first();
    const src = await img.getAttribute('src').catch(() => null);
    if (!src || !src.startsWith('/avatar/')) {
        failures.push(`${label}: avatar src is '${src}'`);
        return null;
    }
    const served = await page.evaluate(async (path) => {
        const res = await fetch(path);
        return { status: res.status, type: res.headers.get('content-type') };
    }, src);
    if (served.status !== 200) failures.push(`${label}: ${src} answers ${served.status}`);
    if (served.type !== 'image/png') failures.push(`${label}: ${src} serves '${served.type}'`);
    const drawn = await page.evaluate(() => {
        const el = document.querySelector('.identity-row img.avatar-photo, .person-head img.avatar-photo');
        if (!el) return null;
        const style = getComputedStyle(el);
        return { width: el.naturalWidth, hidden: style.display === 'none' };
    });
    if (!drawn) {
        failures.push(`${label}: the avatar img left the page`);
        return null;
    }
    if (drawn.hidden) failures.push(`${label}: the photo hid under the initials`);
    if (drawn.width !== PHOTO_WIDTH) {
        failures.push(`${label}: avatar naturalWidth ${drawn.width}, expected ${PHOTO_WIDTH}`);
    }
    return src;
}

let src = null;
if (!failures.length) {
    src = await avatarRendered('settings');
    if (src) await page.screenshot({ path: `${shots}/photo-avatar.png` });
}

// The person page wears the same photo at its own size.
if (!failures.length) {
    const ownId = src.match(/\/avatar\/([^?]+)/)?.[1] || null;
    if (!ownId) {
        failures.push(`no own avatar id in '${src}'`);
    } else {
        await page.goto(`${base}/people/${ownId}`, { waitUntil: 'networkidle' });
        if (!(await page.locator('.person-head').count())) {
            failures.push(`the person page never rendered at ${page.url()}`);
        } else {
            const again = await avatarRendered('person-page');
            if (again && again !== src) failures.push(`the person page serves '${again}', settings serves '${src}'`);
        }
    }
}

// The everyday path: the hide-on-error script registers through __izOwn, so
// the live refresh's morph must keep the rendered photo, not strip it back
// to the initials.
if (!failures.length) {
    await page.evaluate(() => window.__izRefresh());
    // Sleeping is the measurement: the refresh lands without an event.
    await page.waitForTimeout(800);
    await avatarRendered('post-refresh');
}

await browser.close();

note(`page errors ${errors.length ? errors.join(' | ') : 'none'}`);
if (errors.length) failures.push(`page errors: ${errors.join(' | ')}`);
if (failures.length) {
    for (const f of failures) note('FAIL ' + f);
    process.exit(1);
}
note('PASS the avatar proxy renders the im photo on settings and the person page, across a refresh');
