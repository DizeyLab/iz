// The second check `cargo test` cannot make: the moment field's popover
// actually completing a pick in a real engine. `tests/http.rs` asserts the
// shape of the served script; only a browser runs it. This is the class
// that bit twice — `pick()` once threw on a panel with no day input, and
// once lost its `input.value` write entirely: labels updated, saves saved
// nothing, and no wire assertion could see either.
//
// Drives the new-task modal's moment field end to end: open the popover,
// type a time, press a day, and the created card must wear the moment —
// chip and detail field alike — with zero page errors and no document
// overflow while the popover stands open.
//
// Wants the server run.sh leaves behind: the workspace already claimed
// (soft-nav claims it), so this signs in — but it claims too, so the
// script runs standalone as well:
//
//     node crates/izlek-web/tests/browser/moment.mjs http://127.0.0.1:7791
//
// Playwright lives outside the repo, exactly as soft-nav.mjs says.
const { chromium } = await import(process.env.IZLEK_PLAYWRIGHT || 'playwright');

const base = process.argv[2] || 'http://127.0.0.1:7791';
const shots = process.env.SHOT_DIR || '.';
const failures = [];
const note = (line) => console.log(line);

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const pad = (n) => String(n).padStart(2, '0');

const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push('PE ' + e.message));
page.on('console', (m) => {
    if (m.type() === 'error') errors.push('CON ' + m.text());
});

// The workspace is either unclaimed (standalone run: the claim form comes
// up, and filling it signs the admin in) or already claimed by soft-nav
// (the sign-in form comes up for the same admin).
await page.goto(base + '/', { waitUntil: 'networkidle' });
if (await page.locator('input[name="display_name"]').count()) {
    await page.fill('input[name="display_name"]', 'Ada Lovelace');
    await page.fill('input[name="email"]', 'ada@izlek.sh');
    await page.fill('input[name="password"]', 'correct horse battery staple');
    await Promise.all([
        page.waitForLoadState('networkidle'),
        page.click('button[type="submit"]'),
    ]);
} else {
    await page.fill('input[name="email"]', 'ada@izlek.sh');
    await page.fill('input[name="password"]', 'correct horse battery staple');
    await Promise.all([
        page.waitForLoadState('networkidle'),
        page.click('button[type="submit"]'),
    ]);
}
if (await page.locator('.board-stage').count() === 0) {
    failures.push(`signing in landed on ${page.url()} with no board`);
}

// The new-task modal, server-rendered off /?new=1.
await page.goto(base + '/?new=1', { waitUntil: 'networkidle' });
await page.fill('.new-task-form input[name="title"]', 'Moment field holds');

// Open the popover: the label toggles the checkbox, whose change event is
// what makes the datepick script draw the grid.
await page.click('label[for="new-task-deadline"]');
await page.waitForFunction(
    () => document.querySelectorAll('.modal-new-task .datepick-day').length > 0,
    { timeout: 5000 },
).catch(() => failures.push('the day grid never rendered'));

// The overflow check belongs while the popover stands open — that is the
// state the old datetime-local overflowed at.
const overflow = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    client: document.documentElement.clientWidth,
}));
if (overflow.scroll > overflow.client) {
    failures.push(`the open popover overflows the document: ${overflow.scroll} > ${overflow.client}`);
}

// The day to press: tomorrow when it sits in the shown month, else a mid
// month day. The label is computed from the month the grid actually
// rendered, not from today.
const picked = await page.evaluate(() => {
    const panel = document.querySelector('.modal-new-task .datepick-panel');
    const days = [...panel.querySelectorAll('.datepick-day')].map((b) => Number(b.dataset.day));
    const today = new Date();
    const last = new Date(today.getFullYear(), today.getMonth() + 1, 0).getDate();
    const want = today.getDate() + 1 <= last
        ? today.getDate() + 1
        : days[Math.floor(days.length / 2)];
    return { year: Number(panel.dataset.year), month: Number(panel.dataset.month), day: want };
});
const chipLabel = `${MONTHS[picked.month - 1]} ${pad(picked.day)} · 16:20`;
const fieldLabel = `${MONTHS[picked.month - 1]} ${pad(picked.day)} 16:20`;


// The time rides the same panel as two house searchable dropdowns — hour
// and minute. Each is driven like a hand on it: click the trigger, type to
// filter, press the row through its own handler (the panel's scroll-close
// race makes a point-click flake). Neither select carries
// data-autosubmit: a lone pick is half a value, and the commit is the day
// press, which carries both.
for (const [name, value] of [
    ['clock_hour', '16'],
    ['clock_minute', '20'],
]) {
    const trigger = page.locator(
        `.modal-new-task select[name="${name}"] >> xpath=preceding-sibling::button[1]`,
    );
    await trigger.click({ timeout: 5000 });
    const menu = page.locator('.dd-panel.dd-open');
    await menu.locator('.dd-search').fill(value);
    const rowOn = await menu.locator('.dd-search').evaluate((el, want) => {
        const panel = el.closest('.dd-panel');
        const row = panel.querySelector(`.dd-option[data-value="${want}"]`);
        return !!row && !row.classList.contains('dd-option-hidden');
    }, value);
    if (!rowOn) failures.push(`typing '${value}' in the ${name} menu hid its row`);
    await menu.locator(`.dd-option[data-value="${value}"]`).evaluate((el) => el.click());
}
// Then the day press, whose pick() writes the hidden input in place — in
// the modal nothing autosubmits, so the value is read in the same tick as
// the click, before anything can replace the panel.
const written = await page.evaluate((day) => {
    const panel = document.querySelector('.modal-new-task .datepick-panel');
    const input = panel.querySelector('.datepick-input');
    const before = input.value;
    panel.querySelector(`.datepick-day[data-day="${day}"]`).click();
    return { before, after: input.value };
}, picked.day);
if (written.before !== '') {
    failures.push(`the fresh popover carried a day already: ${written.before}`);
}
if (!/^\d{4}-\d{2}-\d{2}$/.test(written.after)) {
    failures.push(`the day pick never wrote the hidden input: '${written.after}'`);
}
const atSubmit = await page.evaluate(() => ({
    hour: document.querySelector('.modal-new-task select[name="clock_hour"]')?.value,
    minute: document.querySelector('.modal-new-task select[name="clock_minute"]')?.value,
    dayInput: !!document.querySelector('.modal-new-task .datepick-input'),
    panel: !!document.querySelector('.modal-new-task .datepick-panel'),
    modal: !!document.querySelector('.modal-new-task'),
    day: document.querySelector('.modal-new-task .datepick-input')?.value,
}));
if (atSubmit.hour !== '16' || atSubmit.minute !== '20') {
    failures.push(`the boxes read '${atSubmit.hour}':${atSubmit.minute}' at submit`);
}
if (atSubmit.day !== written.after) failures.push(`the day box read '${atSubmit.day}' at submit`);
// The board, where the new card must wear the moment. In the modal the
// grid carries data-autosubmit=false — the day pick only writes the
// hidden input, and the form goes on its own button, the way a hand on
// the form would drive it.
await page.click('.new-task-form button[type="submit"]');
const landed = await page
    .waitForFunction(
        () => !document.querySelector('.modal-new-task') && document.querySelector('.board-stage'),
        { timeout: 5000 },
    )
    .then(() => true)
    .catch(() => false);
if (!landed) failures.push('the day pick never landed on the board');
const chipThere = await page
    .waitForFunction(
        (want) => [...document.querySelectorAll('.card-deadline')]
            .some((el) => el.textContent.trim() === want),
        chipLabel,
        { timeout: 5000 },
    )
    .then(() => true)
    .catch(() => false);
// The card opens into the task modal, whose moment field says the same
// day and time in its own voice. A miss dumps the chips it did find.
const chips = await page.evaluate(() =>
    [...document.querySelectorAll('.card-deadline')].map((el) => el.textContent.trim()));
if (!chipThere) failures.push(`no board chip reads '${chipLabel}' — chips: ${chips.join(' | ')}`);
if (chipThere) {
    await page.locator('.card', { hasText: chipLabel }).first().click();
    await page.waitForFunction(
        () => document.querySelector('.detail-mast .datepick-label'),
        { timeout: 5000 },
    ).catch(() => failures.push('the card never opened its task modal'));
    const fieldText = await page
        .locator('.detail-mast .datepick-label')
        .first()
        .textContent()
        .catch(() => '');
    if (fieldText === null || fieldText.trim() !== fieldLabel) {
        failures.push(`the moment field reads '${fieldText}' not '${fieldLabel}'`);
    }
}

await page.screenshot({ path: `${shots}/moment.png`, fullPage: true });
await browser.close();

note(`page errors ${errors.length ? errors.join(' | ') : 'none'}`);
if (errors.length) failures.push(`page errors: ${errors.join(' | ')}`);
if (failures.length) {
    for (const f of failures) note('FAIL ' + f);
    process.exit(1);
}
note(`PASS the moment field holds: a day press wrote ${written.after}, the card wears '${chipLabel}', and the field reads '${fieldLabel}'`);
