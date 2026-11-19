const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(`${__dirname}/alerts.js`, 'utf8');
const key = 'wfmt.riven-alerts.v1';
const rule = (extra = {}) => ({id: 'rule-1', weapon: 'torid', name: 'Torid', max_price: 300, status: 'available', min_reputation: 5, enabled: true, seen: {}, ...extra});
const listing = (extra = {}) => ({auction_id: 'listing-1', weapon: 'torid', weapon_name: 'Torid', price: 300, seller: 'Seller', status: 'ingame', reputation: 5, platform: 'pc', ...extra});
const settle = () => new Promise(resolve => setImmediate(resolve));
function browser({storage = new Map([[key, JSON.stringify({rules: [rule()]})]]), listings = [listing()], permission = 'granted', storageFails = false, noLocks = false} = {}) {
    const notifications = [], timers = [];
    let requests = 0, failFetch = false, failNotification = false;
    class Notification {
        static permission = permission;
        constructor(title, options) {
            if (failNotification) throw Error('unsupported');
            notifications.push({title, options});
        }
    }
    const localStorage = {
        getItem: k => { if (storageFails) throw Error('Storage unavailable'); return storage.get(k) ?? null; },
        setItem: (k, v) => { if (storageFails) throw Error('Storage unavailable'); storage.set(k, v); },
        removeItem: k => storage.delete(k)
    };
    const window = {isSecureContext: true, Notification, addEventListener() {}};
    const context = {window, Notification, localStorage,
        navigator: noLocks ? {} : {locks: {request: async (_key, _options, callback) => callback({})}},
        document: {body: {}, getElementById: () => null},
        MutationObserver: class {observe() {}},
        setInterval: fn => timers.push(fn), setTimeout, structuredClone, AbortSignal,
        fetch: async () => { requests++; if (failFetch) throw Error('Network unavailable'); return {ok: true, json: async () => ({listings, live_scraper: true, server_time: Date.now(), latest_observation: Date.now()})}; }
    };
    vm.runInNewContext(source, context);
    return {storage, notifications, Notification, poll: () => timers[0](), get requests() {return requests;}, failFetch: value => {failFetch = value;}, failNotification: value => {failNotification = value;}};
}

test('inclusive price, status, reputation and weapon matching; once per listing across reloads', async () => {
    const app = browser({listings: [listing(), listing({auction_id: 'expensive', price: 301}), listing({auction_id: 'offline', status: 'offline'}), listing({auction_id: 'rep', reputation: 4}), listing({auction_id: 'weapon', weapon: 'soma'}), listing({auction_id: 'zero', price: 0})]});
    await settle(); assert.equal(app.notifications.length, 1);
    await app.poll(); assert.equal(app.notifications.length, 1);
    const reloaded = browser({storage: app.storage});
    await settle(); assert.equal(reloaded.notifications.length, 0);
});

test('does not consume matches before notification permission is granted', async () => {
    const app = browser({permission: 'default'});
    await settle(); assert.equal(app.notifications.length, 0);
    assert.deepEqual(JSON.parse(app.storage.get(key)).rules[0].seen, {});
    app.Notification.permission = 'granted';
    await app.poll(); assert.equal(app.notifications.length, 1);
});

test('paused rules make no requests; resuming starts monitoring', async () => {
    const storage = new Map([[key, JSON.stringify({rules: [rule({enabled: false})]})]]);
    const app = browser({storage});
    await settle(); assert.equal(app.requests, 0);
    storage.set(key, JSON.stringify({rules: [rule()]}));
    await app.poll(); assert.equal(app.notifications.length, 1);
});

test('storage failures and corrupt data never send unrecorded notifications', async () => {
    const unavailable = browser({storageFails: true});
    const corrupt = browser({storage: new Map([[key, '{broken']])});
    await settle();
    assert.equal(unavailable.requests, 0); assert.equal(corrupt.requests, 0);
    assert.equal(unavailable.notifications.length, 0); assert.equal(corrupt.notifications.length, 0);
});

test('failed notification construction is retried on the next successful check', async () => {
    const app = browser({permission: 'default'});
    await settle(); app.Notification.permission = 'granted'; app.failNotification(true);
    await app.poll(); assert.deepEqual(JSON.parse(app.storage.get(key)).rules[0].seen, {});
    app.failNotification(false); await app.poll(); assert.equal(app.notifications.length, 1);
});

test('network errors recover and fallback lease is released', async () => {
    const app = browser({noLocks: true, permission: 'default'});
    await settle(); app.failFetch(true); await app.poll();
    assert.equal(app.storage.has(`${key}.lease`), false);
    app.failFetch(false); app.Notification.permission = 'granted';
    await app.poll(); assert.equal(app.notifications.length, 1);
});

test('any status accepts unknown reputation only when no minimum is requested', async () => {
    const app = browser({storage: new Map([[key, JSON.stringify({rules: [rule({status: 'any', min_reputation: 0})]})]]), listings: [listing({status: 'offline', reputation: null})]});
    await settle(); assert.equal(app.notifications.length, 1);
});
