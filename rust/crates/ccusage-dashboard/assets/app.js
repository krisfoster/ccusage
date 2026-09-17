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
const BUCKET_MS = 15 * 60 * 1000;

const state = {
	timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC',
	data: null,
};

const SVG_NS = 'http://www.w3.org/2000/svg';

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

function svg(tag, attrs = {}, children = []) {
	const node = document.createElementNS(SVG_NS, tag);
	for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, value);
	for (const child of [].concat(children)) {
		node.append(child instanceof Node ? child : document.createTextNode(String(child)));
	}
	return node;
}

const money = (value) =>
	value >= 1000 ? `$${value.toFixed(0)}` : `$${value.toFixed(2)}`;

/** Axis labels need more precision than totals: a $0.004 day is not $0.00. */
const axisMoney = (value) => {
	if (value === 0) return '$0';
	if (value >= 1000) return `$${Math.round(value).toLocaleString()}`;
	if (value >= 10) return `$${value.toFixed(0)}`;
	if (value >= 1) return `$${value.toFixed(1)}`;
	if (value >= 0.01) return `$${value.toFixed(2)}`;
	return `$${value.toPrecision(1)}`;
};

const perMillion = (value) =>
	value == null ? '—' : value >= 100 ? `$${value.toFixed(0)}` : `$${value.toFixed(2)}`;

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
			const atRate = (count, rate) => (count / 1e6) * rate;
			const cacheWrite = rates.cacheWrite ?? rates.input;
			const cacheRead = rates.cacheRead ?? rates.input;
			if (rates.cacheWrite == null && totals.cacheWriteTokens > 0) inferredCache = true;
			actual += totals.cost;
			projected +=
				atRate(totals.inputTokens, rates.input) +
				atRate(totals.outputTokens, rates.output) +
				atRate(totals.cacheWriteTokens, cacheWrite) +
				atRate(totals.cacheReadTokens, cacheRead);
		}
		if (actual === 0 && projected === 0) continue;
		rows.push({ provider: provider.label, actual, projected, excluded, inferredCache });
	}
	return rows.sort((left, right) => left.projected - right.projected);
}

/**
 * The published price list, as rates rather than as a verdict.
 *
 * Every row links to where its provider publishes the rate: the numbers come
 * from a third-party snapshot, and a price nobody can check is a price nobody
 * should trust.
 */
function priceRows(pricing) {
	return Object.entries(pricing?.models || {})
		.sort((left, right) => left[0].localeCompare(right[0]))
		.map(([model, rates]) => [
			{
				sort: model,
				node: rates.source
					? el('a', { href: rates.source, target: '_blank', rel: 'noopener noreferrer' }, model)
					: el('span', {}, model),
			},
			{ sort: rates.provider || '—' },
			{ sort: rates.tier || '—' },
			{ sort: rates.input ?? null, label: perMillion(rates.input) },
			{ sort: rates.output ?? null, label: perMillion(rates.output) },
			{ sort: rates.cacheWrite ?? null, label: perMillion(rates.cacheWrite) },
			{ sort: rates.cacheRead ?? null, label: perMillion(rates.cacheRead) },
		]);
}

/* --------------------------------------------------------------- rendering */

/**
 * A table the reader can reorder.
 *
 * Rows carry a `sort` value per column so a price sorts by its number and not
 * by the string `"$10.00" < "$9.00"`. The sort is stable — rows tied on the
 * chosen column keep the order the caller gave them — and re-clicking a column
 * reverses it. A cell with no value sorts last whichever way the column runs,
 * since "unpublished" is not a price and does not belong at the top.
 */
function sortableTable(headers, rows) {
	const state = { column: null, descending: true };
	const container = el('div', { class: 'sortable' });

	const render = () => {
		let ordered = rows;
		if (state.column != null) {
			const direction = state.descending ? -1 : 1;
			ordered = rows
				.map((row, index) => ({ row, index }))
				.sort((left, right) => {
					const a = left.row[state.column].sort;
					const b = right.row[state.column].sort;
					if (a == null || b == null) {
						if (a == null && b == null) return left.index - right.index;
						return a == null ? 1 : -1;
					}
					const compared =
						typeof a === 'number' && typeof b === 'number'
							? a - b
							: String(a).localeCompare(String(b));
					return compared !== 0 ? compared * direction : left.index - right.index;
				})
				.map(({ row }) => row);
		}

		const head = el(
			'tr',
			{},
			headers.map((header, column) => {
				const sorted = state.column === column;
				const cell = el(
					'th',
					{
						class: `sortable-th${sorted ? ' sorted' : ''}`,
						role: 'button',
						tabindex: '0',
						'aria-sort': sorted ? (state.descending ? 'descending' : 'ascending') : 'none',
					},
					`${header}${sorted ? (state.descending ? ' ▼' : ' ▲') : ''}`,
				);
				const toggle = () => {
					state.descending = state.column === column ? !state.descending : true;
					state.column = column;
					render();
				};
				cell.addEventListener('click', toggle);
				cell.addEventListener('keydown', (event) => {
					if (event.key === 'Enter' || event.key === ' ') {
						event.preventDefault();
						toggle();
					}
				});
				return cell;
			}),
		);
		const body = el(
			'tbody',
			{},
			ordered.map((row) =>
				el(
					'tr',
					{},
					row.map((cell) => el('td', {}, cell.node ?? String(cell.label ?? cell.sort ?? '—'))),
				),
			),
		);
		container.replaceChildren(el('table', {}, [el('thead', {}, head), body]));
	};

	render();
	return container;
}

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

/**
 * A "nice" axis top: the smallest 1/2/5 × 10ⁿ at or above the peak, so the
 * labels read $2 / $4 / $6 rather than $1.87 / $3.74.
 */
function axisTop(peak) {
	if (!(peak > 0)) return 1;
	const magnitude = 10 ** Math.floor(Math.log10(peak));
	const step = [1, 2, 2.5, 5, 10].find((factor) => peak <= factor * magnitude) ?? 10;
	return step * magnitude;
}

const CHART = { width: 960, height: 220, left: 56, right: 8, top: 10, bottom: 28 };

/**
 * The daily chart, drawn as SVG with both axes.
 *
 * A bar chart without a scale invites the reader to guess it, and every guess
 * is wrong by whatever the peak happens to be. The viewBox does the responsive
 * work: the chart keeps its aspect ratio and its labels stay legible at any
 * width without a resize listener.
 */
function series(days) {
	const { width, height, left, right, top, bottom } = CHART;
	const plotWidth = width - left - right;
	const plotHeight = height - top - bottom;
	const peak = Math.max(...days.map((day) => day.totals.cost), 0);
	const ceiling = axisTop(peak);
	const y = (cost) => top + plotHeight - (cost / ceiling) * plotHeight;

	const ticks = [0, 0.25, 0.5, 0.75, 1].map((fraction) => fraction * ceiling);
	const gridlines = ticks.flatMap((tick) => [
		svg('line', {
			class: 'grid',
			x1: left,
			x2: width - right,
			y1: y(tick).toFixed(1),
			y2: y(tick).toFixed(1),
		}),
		svg(
			'text',
			{ class: 'tick y', x: left - 8, y: (y(tick) + 4).toFixed(1) },
			axisMoney(tick),
		),
	]);

	const slot = plotWidth / Math.max(days.length, 1);
	const barWidth = Math.max(1, Math.min(28, slot * 0.7));
	const bars = days.map((day, index) => {
		const x = left + slot * (index + 0.5) - barWidth / 2;
		const barTop = day.totals.cost > 0 ? Math.min(y(day.totals.cost), top + plotHeight - 1) : top + plotHeight;
		return svg('rect', {
			class: 'bar',
			x: x.toFixed(1),
			y: barTop.toFixed(1),
			width: barWidth.toFixed(1),
			height: Math.max(0, top + plotHeight - barTop).toFixed(1),
			rx: Math.min(3, barWidth / 2).toFixed(1),
		}, svg('title', {}, `${day.date} — ${money(day.totals.cost)}`));
	});

	// Roughly one label per 90px, so a year of days does not become a smear.
	const every = Math.max(1, Math.ceil(days.length / Math.floor(plotWidth / 90)));
	const dates = days
		.map((day, index) => ({ day, index }))
		.filter(({ index }) => index % every === 0 || index === days.length - 1)
		.map(({ day, index }) =>
			svg(
				'text',
				{ class: 'tick x', x: (left + slot * (index + 0.5)).toFixed(1), y: height - 8 },
				day.date.slice(5),
			),
		);

	const axes = [
		svg('line', { class: 'axis', x1: left, x2: left, y1: top, y2: top + plotHeight }),
		svg('line', {
			class: 'axis',
			x1: left,
			x2: width - right,
			y1: top + plotHeight,
			y2: top + plotHeight,
		}),
	];

	return svg(
		'svg',
		{
			viewBox: `0 0 ${width} ${height}`,
			role: 'img',
			'aria-label': `Daily spend, ${days.length} day(s), peak ${money(peak)}`,
		},
		[...gridlines, ...bars, ...dates, ...axes],
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
	document.getElementById('series').replaceChildren(series(days));

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
			? sortableTable(
					['Provider', 'Would have cost', 'Actual (comparable)'],
					comparisons.map((row) => [
						{ sort: row.provider },
						{ sort: row.projected, label: money(row.projected) },
						{ sort: row.actual, label: money(row.actual) },
					]),
				)
			: el('p', { class: 'muted' }, 'No price data was published with this dashboard.'),
	);

	const prices = priceRows(pricing);
	document.getElementById('prices').replaceChildren(
		prices.length > 0
			? sortableTable(
					[
						'Model',
						'Provider',
						'Tier',
						'Input / M',
						'Output / M',
						'Cache write / M',
						'Cache read / M',
					],
					prices,
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

void main();
