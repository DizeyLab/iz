// The drop hand: `tests/http.rs` can assert the shape of the served
// script, but no form post can produce a drag — only a browser builds a
// DataTransfer that carries real File objects, and only its events reach
// the delegated dragover/dragleave/drop listeners on the upload box.
// setInputFiles is no substitute either: it fills the input directly and
// never touches the drag path.
//
// Drives the task detail's file box end to end: dragover must mark the
// box, dragleave for the document body must unmark it, and a drop of two
// files must ride the same change -> requestSubmit -> multipart XHR path
// the picker uses — the landed chips are the proof, with zero page errors.
// The pane legs then repeat the same three over the Files tab's whole
// section, the box's hit area: a drop nowhere near the box still uploads.
//
// Wants the server run.sh leaves behind, plus the session cookie it mints:
// IZ_SESSION_COOKIE carries the sealed token the fake im knows — the same
// minted session soft-nav runs on, so the owner is already provisioned.
// Standalone runs need it too: there is no form left to sign in through:
//
//     node crates/iz-web/tests/browser/drop.mjs http://127.0.0.1:7791
//
// Playwright lives outside the repo, exactly as soft-nav.mjs says.
const { chromium } = await import(process.env.IZ_PLAYWRIGHT || 'playwright');

const base = process.argv[2] || 'http://127.0.0.1:7791';
const shots = process.env.SHOT_DIR || '.';
const failures = [];
const note = (line) => console.log(line);

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

await page.goto(base + '/', { waitUntil: 'networkidle' });
if (await page.locator('.board-stage').count() === 0) {
    failures.push(`signing in landed on ${page.url()} with no board`);
}

// The task to drop onto, made the way a hand does: the new-task modal,
// server-rendered off /?new=1, submitted by its own button.
if (!failures.length) {
    await page.goto(base + '/?new=1', { waitUntil: 'networkidle' });
    await page.fill('.new-task-form input[name="title"]', 'Drop holds');
    await page.click('.new-task-form button[type="submit"]');
    const landed = await page
        .waitForFunction(
            () => !document.querySelector('.modal-new-task') && document.querySelector('.board-stage'),
            { timeout: 5000 },
        )
        .then(() => true)
        .catch(() => false);
    if (!landed) failures.push('the new task never landed on the board');
}

// The card opens into the task detail, whose file box sits behind the
// Files tab — the modal lands on the Task tab, and the tab is a soft-nav
// link, so the box arrives on a second fetch-swap. Earlier scripts in the
// run leave their own cards behind, so take the first.
if (!failures.length) {
    await page.locator('.card-title').first().click();
    const tabsThere = await page
        .waitForFunction(() => document.querySelector('.detail-tabs'), { timeout: 5000 })
        .then(() => true)
        .catch(() => false);
    if (!tabsThere) {
        failures.push(`the task detail never showed its tabs at ${page.url()}`);
    } else {
        await page.locator('a.detail-tab[href$="tab=files"]').click();
        const boxThere = await page
            .waitForFunction(() => document.querySelector('.file-upload-box'), { timeout: 10000 })
            .then(() => true)
            .catch(() => false);
        if (!boxThere) failures.push(`the Files tab never showed the file box at ${page.url()}`);
    }
}

// The hover mark: a dragover carrying real files must paint the box —
// the one state the picker's change path can never reach.
if (!failures.length) {
    const over = await page.evaluate(() => {
        const box = document.querySelector('.file-upload-box');
        if (!box) return null;
        const dt = new DataTransfer();
        dt.items.add(new File(['iz one'], 'dropped-a.txt', { type: 'text/plain' }));
        box.dispatchEvent(new DragEvent('dragover', { bubbles: true, cancelable: true, dataTransfer: dt }));
        return box.classList.contains('file-upload-over');
    });
    if (over !== true) {
        failures.push(`dragover never marked the box (over=${over})`);
    } else {
        // The class lands in one tick, and `color` has no transition —
        // the text wears the accent at once — while border-color rides
        // the ~150ms interpolation. Equality is the settled paint: poll
        // for the border reaching the accent the text already wears.
        const painted = await page
            .waitForFunction(
                () => {
                    const b = document.querySelector('.file-upload-box');
                    if (!b) {
                        return false;
                    }
                    const cs = getComputedStyle(b);
                    return cs.borderColor === cs.color;
                },
                { timeout: 3000 },
            )
            .then(() => true)
            .catch(() => false);
        if (!painted) failures.push('dragover never repainted the box border');
        else await page.screenshot({ path: `${shots}/drop-hover.png` });
    }
}

// Leaving for the body — outside the box — must strip the mark; only a
// relatedTarget still inside the box would rightly keep it.
if (!failures.length) {
    const gone = await page.evaluate(() => {
        const box = document.querySelector('.file-upload-box');
        if (!box) return null;
        const dt = new DataTransfer();
        box.dispatchEvent(new DragEvent('dragleave', {
            bubbles: true, cancelable: true, dataTransfer: dt, relatedTarget: document.body,
        }));
        return box.classList.contains('file-upload-over');
    });
    if (gone !== false) failures.push(`dragleave from outside never unmarked the box (over=${gone})`);
}

// The drop itself: two real files into the box, riding the change ->
// requestSubmit -> multipart XHR path. The chips are the proof the upload
// ran and the detail swapped them in — network work, not a repaint, hence
// the long wait.
if (!failures.length) {
    await page.evaluate(() => {
        const box = document.querySelector('.file-upload-box');
        const dt = new DataTransfer();
        dt.items.add(new File(['iz one'], 'dropped-a.txt', { type: 'text/plain' }));
        dt.items.add(new File(['iz two'], 'dropped-b.txt', { type: 'text/plain' }));
        box.dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: dt }));
    });
    const dropped = await page
        .waitForFunction(
            () => {
                const names = [...document.querySelectorAll('.file-chip-name')]
                    .map((el) => el.textContent.trim()).sort();
                return names.length === 2 && names[0] === 'dropped-a.txt' && names[1] === 'dropped-b.txt';
            },
            { timeout: 10000 },
        )
        .then(() => true)
        .catch(() => false);
    if (!dropped) {
        const chips = await page.evaluate(() =>
            [...document.querySelectorAll('.file-chip-name')].map((el) => el.textContent.trim()));
        failures.push(`the dropped files never landed as chips — chips: ${chips.join(' | ')}`);
    } else {
        await page.screenshot({ path: `${shots}/drop-landed.png` });
    }
}

// The same hand, dropped on the pane instead of the box: the Files tab's
// whole section is the box's hit area, so a drag parked over the list or
// the head marks the box, leaving the pane for the body unmarks it, and a
// drop on the list rides the same change path. Dispatched on the head and
// list nodes — children of the pane — to prove the closest() resolution,
// not a lucky direct hit on the section itself.
if (!failures.length) {
    const paneOver = await page.evaluate(() => {
        const head = document.querySelector('.files-pane .detail-block-head');
        if (!head) return null;
        const dt = new DataTransfer();
        dt.items.add(new File(['iz three'], 'dropped-c.txt', { type: 'text/plain' }));
        head.dispatchEvent(new DragEvent('dragover', { bubbles: true, cancelable: true, dataTransfer: dt }));
        const box = document.querySelector('.file-upload-box');
        return box ? box.classList.contains('file-upload-over') : null;
    });
    if (paneOver !== true) {
        failures.push(`a dragover on the pane never marked the box (over=${paneOver})`);
    }
}

if (!failures.length) {
    const paneGone = await page.evaluate(() => {
        const head = document.querySelector('.files-pane .detail-block-head');
        if (!head) return null;
        head.dispatchEvent(new DragEvent('dragleave', {
            bubbles: true, cancelable: true, relatedTarget: document.body,
        }));
        const box = document.querySelector('.file-upload-box');
        return box ? box.classList.contains('file-upload-over') : null;
    });
    if (paneGone !== false) failures.push(`dragleave off the pane never unmarked the box (over=${paneGone})`);
}

if (!failures.length) {
    await page.evaluate(() => {
        const list = document.querySelector('.files-pane .file-list');
        if (!list) return;
        const dt = new DataTransfer();
        dt.items.add(new File(['iz three'], 'dropped-c.txt', { type: 'text/plain' }));
        list.dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: dt }));
    });
    const paneDropped = await page
        .waitForFunction(
            () => {
                const names = [...document.querySelectorAll('.file-chip-name')]
                    .map((el) => el.textContent.trim()).sort();
                return names.length === 3 && names[2] === 'dropped-c.txt';
            },
            { timeout: 10000 },
        )
        .then(() => true)
        .catch(() => false);
    if (!paneDropped) {
        const chips = await page.evaluate(() =>
            [...document.querySelectorAll('.file-chip-name')].map((el) => el.textContent.trim()));
        failures.push(`a drop on the pane never landed as a chip — chips: ${chips.join(' | ')}`);
    } else {
        await page.screenshot({ path: `${shots}/drop-pane.png` });
    }
}

await browser.close();

note(`page errors ${errors.length ? errors.join(' | ') : 'none'}`);
if (errors.length) failures.push(`page errors: ${errors.join(' | ')}`);
if (failures.length) {
    for (const f of failures) note('FAIL ' + f);
    process.exit(1);
}
note('PASS the drop rides the real upload path: dragover marked the box, dragleave unmarked it, both dropped files landed as chips, and the pane itself caught a third');
