/*
 * The dashboard is a single dependency-free script on purpose.
 *
 * It is served from a bucket prefix that anyone can read, so every byte here is
 * public: it holds no usage data, no credentials, and no bucket names. The
 * numbers arrive at runtime either from signed URLs carried in the location
 * fragment (never the query string, which would put a bearer token into server
 * logs and browser history) or from a localhost server the CLI runs against the
 * user's own credentials.
 */

const ROLLUP_SCHEMA = 1;
const BUCKETS_PER_DAY = 96;
const BUCKET_MS = 15 * 60 * 1000;

const state = {
	timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC',
	data: null,
};

function el(tag, attrs = {}, children = []) {
	const node = document.createElement(tag);
	for (const [name, value] of Object.entries(attrs)) {
		if (name === 'class') node.className = value;
		else if (name === 'title') node.title = value;
		else node.setAttribute(name, value);
	}
	for (const child of [].concat(children)) {
		node.append(child instanceof Node ? child : document.createTextNode(String(child)));
	}
	return node;
}

const money = (value) =>
	value >= 1000 ? `$${value.toFixed(0)}` : `$${value.toFixed(2)}`;

const tokens = (value) => {
	if (value >= 1e9) return `${(value / 1e9).toFixed(2)}B`;
	if (value >= 1e6) return `${(value / 1e6).toFixed(2)}M`;
	if (value >= 1e3) return `${(value / 1e3).toFixed(1)}k`;
	return String(value);
};

/* ---------------------------------------------------------------- loading */

/** Signed URLs ride in the fragment so they never reach a server log. */
function fragmentSources() {
	const fragment = location.hash.replace(/^#/, '');
	if (!fragment) return null;
	const encoded = new URLSearchParams(fragment).get('s');
	if (!encoded) return null;
	try {
		const json = atob(encoded.replace(/-/g, '+').replace(/_/g, '/'));
		return JSON.parse(json);
	} catch {
		return null;
	}
}

async function fetchJson(url, { optional = false } = {}) {
	const response = await fetch(url, { cache: 'no-store' });
	if (!response.ok) {
		if (optional) return null;
		const reason =
			response.status === 403 || response.status === 401
				? 'the link is not authorized (it may have expired)'
				: `the server answered ${response.status}`;
		throw new Error(`Could not load ${url.split('?')[0]}: ${reason}.`);
	}
	return response.json();
}

async function load() {
	const signed = fragmentSources();
	const at = (name) => (signed && signed[name]) || `data/${name}.json`;
	const [daily, weekly, monthly, models, pricing, equivalence] = await Promise.all([
		fetchJson(at('daily')),
		fetchJson(at('weekly'), { optional: true }),
		fetchJson(at('monthly'), { optional: true }),
		fetchJson(at('models'), { optional: true }),
		fetchJson('pricing.json', { optional: true }),
		fetchJson('model-equivalence.json', { optional: true }),
	]);
	if (daily && daily.schema > ROLLUP_SCHEMA) {
		throw new Error(
			`This bucket was written by a newer ccusage (rollup schema ${daily.schema}); upgrade to read it.`,
		);
	}
	return { daily, weekly, monthly, models, pricing, equivalence, expiresAt: signed?.expiresAt };
}

/* ------------------------------------------------------------ re-bucketing */

/**
 * Cells are stored in 15-minute UTC buckets precisely so a viewer can re-cut
 * the day locally: a `+05:45` offset or a DST jump moves cells between local
 * days without the stored data being wrong.
 */
function localDays(daily, timezone) {
	const formatter = new Intl.DateTimeFormat('en-CA', {
		timeZone: timezone,
		year: 'numeric',
		month: '2-digit',
		day: '2-digit',
	});
	const days = new Map();
	for (const [utcDate, cells] of Object.entries(daily.days || {})) {
		const midnight = Date.parse(`${utcDate}T00:00:00Z`);
		for (const cell of cells) {
			if (cell.suppressed) continue;
			const at = new Date(midnight + (cell.i ?? 0) * BUCKET_MS);
			const day = formatter.format(at);
			const bucket = days.get(day) || { date: day, cells: [], totals: emptyTotals() };
			bucket.cells.push(cell);
			addCell(bucket.totals, cell);
			days.set(day, bucket);
		}
	}
	return [...days.values()].sort((left, right) => left.date.localeCompare(right.date));
}

const emptyTotals = () => ({
	inputTokens: 0,
	outputTokens: 0,
	cacheWriteTokens: 0,
	cacheReadTokens: 0,
	cost: 0,
	messages: 0,
});

function addCell(totals, cell) {
	totals.inputTokens += cell.in || 0;
	totals.outputTokens += cell.out || 0;
	totals.cacheWriteTokens += cell.cw || 0;
	totals.cacheReadTokens += cell.cr || 0;
	totals.cost += cell.cost || 0;
	totals.messages += cell.msgs || 0;
	return totals;
}

const totalTokens = (totals) =>
	totals.inputTokens + totals.outputTokens + totals.cacheWriteTokens + totals.cacheReadTokens;

function groupBy(cells, pick) {
	const groups = new Map();
	for (const cell of cells) {
		const key = pick(cell) || 'unknown';
		groups.set(key, addCell(groups.get(key) || emptyTotals(), cell));
	}
	return [...groups.entries()].sort((left, right) => right[1].cost - left[1].cost);
}

/* ------------------------------------------------------------ counterfactual */

/**
 * Mirrors `ccusage compare`: tokens are repriced at the equivalent model's
 * published rate, and a model with no equivalent or no published price is
 * dropped from *both* sides rather than compared against a full bill.
 */
function counterfactuals(modelTotals, equivalence, pricing) {
	if (!equivalence || !pricing) return [];
	const tierOf = (model) => {
		const name = model.toLowerCase();
		let best = null;
		for (const tier of equivalence.tiers || []) {
			for (const pattern of tier.matches || []) {
				if (name.includes(pattern.toLowerCase()) && (!best || pattern.length > best.length)) {
					best = { length: pattern.length, tier };
				}
			}
		}
		return best?.tier ?? null;
	};

	const rows = [];
	for (const provider of equivalence.providers || []) {
		let actual = 0;
		let projected = 0;
		let excluded = 0;
		let inferredCache = false;
		for (const [model, totals] of Object.entries(modelTotals)) {
			const target = tierOf(model)?.models?.[provider.id];
			const rates = target ? pricing.models?.[target] : null;
			if (!target || !rates) {
				excluded += totals.cost;
				continue;
			}
			const perMillion = (count, rate) => (count / 1e6) * rate;
			const cacheWrite = rates.cacheWrite ?? rates.input;
			const cacheRead = rates.cacheRead ?? rates.input;
			if (rates.cacheWrite == null && totals.cacheWriteTokens > 0) inferredCache = true;
			actual += totals.cost;
			projected +=
				perMillion(totals.inputTokens, rates.input) +
				perMillion(totals.outputTokens, rates.output) +
				perMillion(totals.cacheWriteTokens, cacheWrite) +
				perMillion(totals.cacheReadTokens, cacheRead);
		}
		if (actual === 0 && projected === 0) continue;
		rows.push({
			provider: provider.label,
			actual,
			projected,
			saving: actual - projected,
			savingPercent: actual > 0 ? ((actual - projected) / actual) * 100 : 0,
			excluded,
			inferredCache,
		});
	}
	return rows.sort((left, right) => right.saving - left.saving);
}

/* --------------------------------------------------------------- rendering */

function table(headers, rows) {
	const head = el(
		'thead',
		{},
		el(
			'tr',
			{},
			headers.map((header) => el('th', {}, header)),
		),
	);
	const body = el(
		'tbody',
		{},
		rows.map((row) =>
			el(
				'tr',
				{},
				row.map((cell) => el('td', {}, cell)),
			),
		),
	);
	return el('table', {}, [head, body]);
}

function totalsTiles(totals, dayCount) {
	const tiles = [
		['Spend', money(totals.cost)],
		['Tokens', tokens(totalTokens(totals))],
		['Messages', totals.messages.toLocaleString()],
		['Days with usage', String(dayCount)],
		['Average per day', dayCount ? money(totals.cost / dayCount) : '$0.00'],
	];
	return tiles.map(([label, value]) =>
		el('div', { class: 'tile' }, [el('div', { class: 'value' }, value), el('div', { class: 'label' }, label)]),
	);
}

function series(days) {
	const peak = Math.max(...days.map((day) => day.totals.cost), 0.0001);
	return days.map((day) =>
		el('div', {
			class: 'bar-col',
			title: `${day.date} — ${money(day.totals.cost)}`,
			style: `height:${Math.max(2, (day.totals.cost / peak) * 100)}%`,
		}),
	);
}

function totalsRows(entries) {
	return entries.map(([name, totals]) => [
		name,
		money(totals.cost),
		tokens(totalTokens(totals)),
		totals.messages.toLocaleString(),
	]);
}

function notices(daily, cells) {
	const items = [];
	for (const anomaly of daily.anomalies || []) {
		items.push(
			`${anomaly.kind === 'lateEdit' ? 'Late edit' : anomaly.kind}: ${anomaly.machineId} / ${anomaly.agent} / ${anomaly.utcDate}${
				anomaly.detail ? ` — ${anomaly.detail}` : ''
			}`,
		);
	}
	const suppressed = (Object.values(daily.days || {}).flat() || []).filter((cell) => cell.suppressed);
	if (suppressed.length > 0) {
		items.push(
			`${suppressed.length} cell(s) another machine had already reported are excluded from these totals.`,
		);
	}
	const suspected = cells.filter((cell) => (cell.notes || []).length > 0).length;
	if (suspected > 0) {
		items.push(`${suspected} cell(s) partially overlap another machine and are counted in full.`);
	}
	return items;
}

function render() {
	const { daily, weekly, monthly, models, pricing, equivalence, expiresAt } = state.data;
	const days = localDays(daily, state.timezone);
	const cells = days.flatMap((day) => day.cells);
	const totals = cells.reduce((accumulated, cell) => addCell(accumulated, cell), emptyTotals());

	document.getElementById('totals').replaceChildren(...totalsTiles(totals, days.length));
	document.getElementById('series').replaceChildren(...series(days));

	const modelTotals = Object.fromEntries(groupBy(cells, (cell) => cell.m));
	const columns = ['', 'Cost', 'Tokens', 'Messages'];
	document
		.getElementById('models')
		.replaceChildren(table(['Model', ...columns.slice(1)], totalsRows(Object.entries(modelTotals))));
	document
		.getElementById('machines')
		.replaceChildren(table(['Machine', ...columns.slice(1)], totalsRows(groupBy(cells, (cell) => cell.machine))));
	document
		.getElementById('agents')
		.replaceChildren(table(['Agent', ...columns.slice(1)], totalsRows(groupBy(cells, (cell) => cell.agent))));

	const periods = (rollup) =>
		table(
			['Period', 'Cost', 'Tokens', 'Messages'],
			(rollup?.periods || []).map((period) => [
				period.period,
				money(period.totals.cost),
				tokens(totalTokens(period.totals)),
				period.totals.messages.toLocaleString(),
			]),
		);
	document.getElementById('weekly').replaceChildren(periods(weekly));
	document.getElementById('monthly').replaceChildren(periods(monthly));

	const items = notices(daily, cells);
	document
		.getElementById('notices')
		.replaceChildren(
			...(items.length > 0
				? items.map((item) => el('div', { class: 'notice' }, item))
				: [el('p', { class: 'muted' }, 'Nothing to flag: no late edits and no cross-machine duplicates.')]),
		);

	const comparisons = counterfactuals(modelTotals, equivalence, pricing);
	document.getElementById('compare').replaceChildren(
		comparisons.length > 0
			? table(
					['Provider', 'Would have cost', 'Actual (comparable)', 'Saving', 'Saving %'],
					comparisons.map((row) => [
						row.provider,
						money(row.projected),
						money(row.actual),
						el('span', { class: row.saving >= 0 ? 'saving' : '' }, money(row.saving)),
						`${row.savingPercent.toFixed(1)}%`,
					]),
				)
			: el('p', { class: 'muted' }, 'No price data was published with this dashboard.'),
	);
	document.getElementById('compare-note').textContent = comparisons.some((row) => row.inferredCache)
		? 'Standard list rates only. Some targets publish no cache-write price, so those tokens are charged at their input rate. Models with no equivalent or no published price are excluded from both columns.'
		: 'Standard list rates only: long-context tiers, batch rates, and subscription plans are not applied. Models with no equivalent or no published price are excluded from both columns.';

	const generated = daily.generatedAt ? new Date(daily.generatedAt) : null;
	document.getElementById('freshness').textContent = [
		generated ? `synced ${generated.toLocaleString()}` : '',
		expiresAt ? `link expires ${new Date(expiresAt).toLocaleString()}` : '',
	]
		.filter(Boolean)
		.join(' · ');
	document.getElementById('attribution').textContent = pricing?.attribution || '';
	document.getElementById('app').hidden = false;
	document.getElementById('gate').hidden = true;

	void models;
}

function timezones() {
	const supported =
		typeof Intl.supportedValuesOf === 'function' ? Intl.supportedValuesOf('timeZone') : [];
	const unique = [...new Set(['UTC', state.timezone, ...supported])];
	const select = document.getElementById('timezone');
	select.replaceChildren(...unique.map((zone) => el('option', { value: zone }, zone)));
	select.value = state.timezone;
	select.addEventListener('change', () => {
		state.timezone = select.value;
		render();
	});
}

async function main() {
	timezones();
	try {
		state.data = await load();
		render();
	} catch (error) {
		document.getElementById('gate-detail').textContent = error.message;
	}
}

main();
