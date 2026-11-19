// The shell owns this controller so navigation does not stop monitoring.
(() => {
    if (window.wfmtAlerts) return;
    const KEY = 'wfmt.riven-alerts.v1';
    const STATUSES = {any: 'Any status', available: 'Online or in game', online: 'Online', ingame: 'In game'};
    const SELLER_STATUSES = {online: 'Online', ingame: 'In game', offline: 'Offline'};
    let root, state = {rules: []}, error = '', status = 'Checking…', busy = false;
    let matches = [], permissionError = '';
    let displayedRules, displayedMatches;
    const el = (tag, text, cls) => {
        const node = document.createElement(tag);
        if (text !== undefined) node.textContent = text;
        if (cls) node.className = cls;
        return node;
    };
    const plat = n => `${n.toLocaleString()}p`;
    // Same image path and initial fallback as the Rust WeaponImage component.
    function weaponImage(slug, name) {
        const img = el('img', undefined, 'weapon-img sm');
        img.src = `/img/weapon/${encodeURIComponent(slug)}.png`;
        img.alt = ''; img.loading = 'lazy';
        img.onerror = () => img.replaceWith(el('div', name.charAt(0) || '?', 'weapon-img placeholder sm'));
        return img;
    }
    function row({slug, name, tag, meta, priceLabel, price, actions, muted}) {
        const node = el('div', undefined, muted ? 'alert-row paused' : 'alert-row');
        const text = el('div', undefined, 'alert-row-text');
        const title = el('strong', name);
        if (tag) title.append(el('span', tag, 'tag alert-tag'));
        text.append(title, el('p', meta));
        const value = el('div', undefined, 'alert-row-price');
        if (priceLabel) value.append(el('span', priceLabel, 'muted small'));
        value.append(el('span', plat(price), 'plat'));
        const actionBox = el('div', undefined, 'alert-row-actions');
        actionBox.append(...actions);
        node.append(weaponImage(slug, name), text, value, actionBox);
        return node;
    }
    function read() {
        const parsed = JSON.parse(localStorage.getItem(KEY) || '{"rules":[]}');
        if (!parsed || !Array.isArray(parsed.rules) || parsed.rules.length > 50 || parsed.rules.some(r =>
            !r || typeof r.id !== 'string' || typeof r.weapon !== 'string' || typeof r.name !== 'string' ||
            !Number.isSafeInteger(r.max_price) || r.max_price <= 0 || !Object.hasOwn(STATUSES, r.status) ||
            !Number.isSafeInteger(r.min_reputation) || r.min_reputation < 0 || typeof r.enabled !== 'boolean' ||
            !r.seen || typeof r.seen !== 'object' || Array.isArray(r.seen))) {
            throw Error('Saved alerts are invalid. Clear this site’s browser storage to reset them.');
        }
        return parsed;
    }
    function load() {
        try { state = read(); error = ''; return true; }
        catch (e) { error = `Could not read browser storage: ${e.message}`; return false; }
    }
    function save(next) {
        try { localStorage.setItem(KEY, JSON.stringify(next)); state = next; error = ''; return true; }
        catch (e) { error = `Could not save alerts: ${e.message}`; return false; }
    }
    // Web Locks serialize polling and rule changes across tabs on HTTPS/localhost.
    async function locked(fn, skip = false) {
        if (navigator.locks) return navigator.locks.request(KEY, {ifAvailable: skip}, lock => lock ? fn() : undefined);
        // Older browsers use a short lease; a crashed tab cannot hold it forever.
        const leaseKey = `${KEY}.lease`;
        const token = `${Date.now()}-${Math.random()}`;
        while (true) {
            let lease;
            try { lease = JSON.parse(localStorage.getItem(leaseKey) || 'null'); } catch { lease = null; }
            if (lease && lease.until > Date.now()) {
                if (skip) return;
                await new Promise(resolve => setTimeout(resolve, 100));
                continue;
            }
            localStorage.setItem(leaseKey, JSON.stringify({token, until: Date.now() + 60000}));
            if (JSON.parse(localStorage.getItem(leaseKey) || 'null')?.token === token) break;
            if (skip) return;
        }
        try { return await fn(); }
        finally {
            if (JSON.parse(localStorage.getItem(leaseKey) || 'null')?.token === token) localStorage.removeItem(leaseKey);
        }
    }
    function permission() {
        if (!window.isSecureContext) return {state: 'blocked', detail: 'Notifications need HTTPS or localhost. Matches still show below.'};
        if (!('Notification' in window)) return {state: 'blocked', detail: 'This browser doesn’t support notifications. Matches still show below.'};
        if (Notification.permission === 'denied') return {state: 'blocked', detail: 'Notifications are blocked in your browser settings. Matches still show below.'};
        if (Notification.permission === 'granted') return {state: 'on', detail: 'Notifications are on. Alerts check every 30 seconds while a tracker tab is open.'};
        return {state: 'prompt', detail: 'Turn on notifications to get pinged. Alerts only check while a tracker tab is open.', canEnable: true};
    }
    function render() {
        if (!root?.isConnected) return;
        root.querySelector('#alerts-error').textContent = error;
        const checkStatus = root.querySelector('#alerts-status');
        checkStatus.textContent = status;
        checkStatus.hidden = !state.rules.some(r => r.enabled);
        const notifications = permission();
        root.querySelector('#alerts-notice').dataset.state = permissionError ? 'blocked' : notifications.state;
        root.querySelector('#alerts-permission-status').textContent = permissionError || notifications.detail;
        const active = state.rules.filter(r => r.enabled).length, paused = state.rules.length - active;
        root.querySelector('#alerts-rules-count').textContent = state.rules.length
            ? [`${active} active`, paused && `${paused} paused`].filter(Boolean).join(' · ') : '';
        const enableButton = root.querySelector('#alerts-permission');
        enableButton.hidden = !notifications.canEnable;
        enableButton.disabled = !notifications.canEnable;
        // Polling should not replace focused controls or reannounce unchanged matches.
        const rulesKey = JSON.stringify(state.rules.map(({seen, ...rule}) => rule));
        if (displayedRules !== rulesKey) {
            displayedRules = rulesKey;
            renderRules();
        }
        const visibleMatches = matches.filter(listing => state.rules.some(rule => matchesRule(rule, listing)));
        const matchesKey = JSON.stringify(visibleMatches);
        if (displayedMatches !== matchesKey) {
            displayedMatches = matchesKey;
            renderMatches(visibleMatches);
        }
    }
    function renderRules() {
        const rules = root.querySelector('#alerts-rules');
        rules.replaceChildren();
        if (!state.rules.length) {
            rules.append(el('p', 'No alerts yet. Pick a weapon and a max price above, or use Create alert on any weapon page.', 'alert-empty'));
        }
        for (const rule of state.rules) {
            const details = [STATUSES[rule.status]];
            if (rule.min_reputation > 0) details.push(`Reputation ≥ ${rule.min_reputation}`);
            const actions = [];
            for (const [action, label] of [['toggle', rule.enabled ? 'Pause' : 'Resume'], ['delete', 'Delete']]) {
                const button = el('button', label, action === 'delete' ? 'alert-delete' : undefined);
                button.type = 'button';
                button.setAttribute('aria-label', `${label} ${rule.name} alert at ${rule.max_price} platinum`);
                button.onclick = () => locked(() => {
                    if (!load()) return render();
                    const next = structuredClone(state);
                    if (action === 'delete') next.rules = next.rules.filter(r => r.id !== rule.id);
                    else { const found = next.rules.find(r => r.id === rule.id); if (found) found.enabled = !found.enabled; }
                    save(next); render();
                }).then(poll).catch(fail);
                actions.push(button);
            }
            rules.append(row({slug: rule.weapon, name: rule.name, tag: rule.enabled ? '' : 'Paused', meta: details.join(' · '),
                priceLabel: 'up to', price: rule.max_price, actions, muted: !rule.enabled}));
        }
    }
    function renderMatches(visibleMatches) {
        const list = root.querySelector('#alerts-matches');
        list.replaceChildren();
        if (!visibleMatches.length) {
            list.append(el('p', state.rules.some(r => r.enabled)
                ? 'Nothing matches right now. New listings that match show up here.'
                : 'Listings that match your alerts show up here.', 'alert-empty'));
        }
        for (const listing of visibleMatches) {
            const link = el('a', 'View listing ↗', 'alert-market-link');
            link.href = `https://warframe.market/auction/${encodeURIComponent(listing.auction_id)}`;
            link.target = '_blank'; link.rel = 'noopener noreferrer';
            link.setAttribute('aria-label', `View ${listing.weapon_name} listing on Warframe Market (opens in a new tab)`);
            const details = [listing.seller, SELLER_STATUSES[listing.status] || 'Status unknown', listing.platform,
                `Reputation ${listing.reputation ?? 'unknown'}`];
            list.append(row({slug: listing.weapon, name: listing.weapon_name, meta: details.filter(Boolean).join(' · '),
                price: listing.price, actions: [link]}));
        }
    }
    function fail(e) { error = e.message || String(e); render(); }
    function mount() {
        const next = document.getElementById('riven-alerts');
        if (!next || next === root) return;
        root = next;
        displayedRules = displayedMatches = undefined;
        root.querySelector('#alerts-permission').onclick = async () => {
            try {
                // Called directly by the click, as required by notification permissions.
                await Notification.requestPermission(); permissionError = ''; render(); poll();
            } catch (e) { permissionError = `Could not enable notifications: ${e.message}`; render(); }
        };
        const form = root.querySelector('#alert-form');
        const sellerStatus = form.querySelector('#alert-seller-status');
        const reputation = form.querySelector('#alert-reputation');
        const saved = root.querySelector('#alerts-saved');
        reputation.addEventListener('invalid', () => {
            reputation.closest('details').open = true;
        });
        function updateFilters() {
            const minRep = Number(reputation.value);
            root.querySelector('#alert-filter-summary').textContent = minRep > 0
                ? `${STATUSES[sellerStatus.value]} · reputation ≥ ${minRep}`
                : STATUSES[sellerStatus.value];
        }
        form.oninput = () => {
            saved.textContent = '';
            updateFilters();
        };
        form.onsubmit = event => {
            event.preventDefault();
            if (!form.reportValidity()) return;
            const {weapon, weaponName} = form.dataset;
            if (!weapon || !weaponName) return;
            const rule = {
                id: crypto.randomUUID ? crypto.randomUUID() : `${Date.now()}-${Math.random()}`,
                weapon, name: weaponName,
                max_price: Number(form.querySelector('#alert-price').value),
                status: sellerStatus.value,
                min_reputation: Number(reputation.value),
                enabled: true, seen: {}
            };
            locked(() => {
                if (!load()) return render();
                if (state.rules.some(r => r.weapon === rule.weapon && r.max_price === rule.max_price &&
                    r.status === rule.status && r.min_reputation === rule.min_reputation)) {
                    error = 'This alert is already in your watchlist.';
                    return render();
                }
                if (state.rules.length >= 50) { error = 'You can save up to 50 alerts.'; return render(); }
                const next = structuredClone(state); next.rules.push(rule);
                if (!save(next)) return render();
                saved.textContent = `Alert saved for ${rule.name}.`;
                form.querySelector('#alert-price').value = '';
                status = 'Checking…';
                render();
                return true;
            }).then(added => { if (added) poll(); }).catch(fail);
        };
        load(); render(); poll();
    }
    function matchesRule(rule, listing) {
        return rule.enabled && rule.weapon === listing.weapon && listing.price > 0 && listing.price <= rule.max_price &&
            (rule.status === 'any' || rule.status === listing.status || (rule.status === 'available' && ['online', 'ingame'].includes(listing.status))) &&
            (rule.min_reputation === 0 || (listing.reputation !== null && listing.reputation >= rule.min_reputation));
    }
    async function check() {
        if (!load()) return render();
        const rules = state.rules.filter(r => r.enabled);
        if (!rules.length) { matches = []; status = ''; render(); return; }
        const response = await fetch('/api/alerts/matches', {
            method: 'POST', headers: {'Content-Type': 'application/json'},
            body: JSON.stringify(rules.map(({weapon, max_price, status, min_reputation}) => ({weapon, max_price, status, min_reputation}))),
            signal: AbortSignal.timeout(30000)
        });
        if (!response.ok) throw Error(`Could not check alerts (${response.status}). Retrying in 30 seconds.`);
        const data = await response.json();
        // A rule may have been edited by another tab during the network request.
        if (!load()) return render();
        matches = data.listings.filter(l => state.rules.some(r => matchesRule(r, l)))
            .sort((a, b) => b.observed_at - a.observed_at || a.price - b.price);
        const age = data.latest_observation === null ? Infinity : data.server_time - data.latest_observation;
        const checked = new Date().toLocaleTimeString([], {hour: 'numeric', minute: '2-digit'});
        status = !data.live_scraper ? `Live scraping is off · checked ${checked}` :
            age > 15 * 60000 ? `Scraped data is stale · checked ${checked}` :
            `Checked ${checked} · every 30 seconds`;
        if ('Notification' in window && Notification.permission === 'granted') {
            const next = structuredClone(state);
            const pending = [];
            for (const rule of next.rules) {
                for (const listing of matches) {
                    if (!matchesRule(rule, listing) || Object.hasOwn(rule.seen, listing.auction_id)) continue;
                    rule.seen[listing.auction_id] = Date.now();
                    pending.push({rule, listing});
                }
            }
            // Save before sending so another tab or a reload cannot repeat these pings.
            if (pending.length && save(next)) {
                for (const {rule, listing} of pending) {
                    try {
                        const note = new Notification(`${listing.weapon_name} Riven · ${listing.price} platinum`, {
                            body: [listing.seller, SELLER_STATUSES[listing.status] || 'Status unknown', listing.platform].filter(Boolean).join(' · '),
                            tag: `wfmt-${rule.id}-${listing.auction_id}`
                        });
                        note.onclick = () => { window.open(`https://warframe.market/auction/${encodeURIComponent(listing.auction_id)}`, '_blank', 'noopener'); note.close(); };
                    } catch (e) {
                        delete rule.seen[listing.auction_id]; save(next);
                        permissionError = `Notification delivery failed: ${e.message}`;
                    }
                }
            }
        }
        render();
    }
    async function poll() {
        if (busy) return;
        busy = true;
        try { await locked(check, true); } catch (e) { fail(e); }
        finally { busy = false; }
    }
    window.wfmtAlerts = {mount};
    window.addEventListener('storage', event => { if (event.key === KEY || event.key === null) { load(); render(); } });
    new MutationObserver(mount).observe(document.body, {childList: true, subtree: true});
    setInterval(poll, 30000);
    load(); mount(); poll();
})();
