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
	filters: {},
	chartTable: false,
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
	const [daily, weekly, monthly, models, machines, sessions, pricing, equivalence] = await Promise.all([
		fetchJson(at('daily')),
		fetchJson(at('weekly'), { optional: true }),
		fetchJson(at('monthly'), { optional: true }),
		fetchJson(at('models'), { optional: true }),
		// Only the locally hosted dashboard serves this; a published one shows machine ids.
		fetchJson(at('machines'), { optional: true }),
		// Which project and session spent what. Only the locally hosted dashboard has it.
		fetchJson(at('sessions'), { optional: true }),
		fetchJson('pricing.json', { optional: true }),
		fetchJson('model-equivalence.json', { optional: true }),
	]);
	if (daily && daily.schema > ROLLUP_SCHEMA) {
		throw new Error(
			`This bucket was written by a newer ccusage (rollup schema ${daily.schema}); upgrade to read it.`,
		);
	}
	return { daily, weekly, monthly, models, machines, sessions, pricing, equivalence, expiresAt: signed?.expiresAt };
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

/* ----------------------------------------------------------------- filters */

/**
 * What the reader has chosen to look at, kept in the query string so a view can be bookmarked or sent.
 * (The fragment is taken: signed links carry their data sources there.) None of it is sensitive.
 */
const FILTER_DEFAULTS = { machine: '', agent: '', model: '', project: '', from: '', to: '', stack: 'none' };
const FILTER_KEYS = Object.keys(FILTER_DEFAULTS);
const STACKS = ['none', 'machine', 'agent', 'model'];

function readFilters() {
	const query = new URLSearchParams(location.search);
	state.filters = { ...FILTER_DEFAULTS };
	for (const key of FILTER_KEYS) {
		const value = query.get(key);
		if (value) state.filters[key] = value;
	}
	if (!STACKS.includes(state.filters.stack)) state.filters.stack = 'none';
}

function writeFilters() {
	const query = new URLSearchParams();
	for (const key of FILTER_KEYS) {
		if (state.filters[key] && state.filters[key] !== FILTER_DEFAULTS[key]) query.set(key, state.filters[key]);
	}
	const search = query.toString();
	history.replaceState(null, '', `${location.pathname}${search ? `?${search}` : ''}${location.hash}`);
}

function setFilters(changes) {
	Object.assign(state.filters, changes);
	writeFilters();
	render();
}

const toggleFilter = (key, value) => setFilters({ [key]: state.filters[key] === value ? '' : value });

const SLICERS = ['machine', 'agent', 'model', 'project', 'from', 'to'];
const isFiltered = () => SLICERS.some((key) => state.filters[key]);

const PICK = {
	machine: (cell) => cell.machine,
	agent: (cell) => cell.agent,
	model: (cell) => cell.m,
};

/** Slices the days the page is showing; every panel reads the result, so the numbers always agree. */
function applyFilters(allDays) {
	const { machine, agent, model, from, to } = state.filters;
	return allDays
		.filter((day) => (!from || day.date >= from) && (!to || day.date <= to))
		.map((day) => {
			const cells = day.cells.filter(
				(cell) =>
					(!machine || cell.machine === machine) &&
					(!agent || cell.agent === agent) &&
					(!model || cell.m === model),
			);
			return { date: day.date, cells, totals: cells.reduce(addCell, emptyTotals()) };
		})
		.filter((day) => day.cells.length > 0);
}

/* ----------------------------------------------------------- colour and stacks */

const SERIES_SLOTS = 8;

/**
 * An entity's colour is fixed by where it ranks across everything the dashboard holds, never across what
 * the current filter happens to leave, so choosing a sandbox does not repaint the others. Past the eighth
 * the tail shares one neutral rather than a ninth hue nobody could tell apart.
 */
function rankings(allCells) {
	return Object.fromEntries(
		Object.entries(PICK).map(([dimension, pick]) => [dimension, groupBy(allCells, pick).map(([name]) => name)]),
	);
}

function slotOf(dimension, name) {
	const index = state.rankings[dimension].indexOf(name);
	return index >= 0 && index < SERIES_SLOTS ? index : SERIES_SLOTS;
}

const slotColor = (slot) => (slot < SERIES_SLOTS ? `var(--series-${slot + 1})` : 'var(--series-other)');

function displayName(dimension, id) {
	if (dimension !== 'machine') return id;
	const machine = (state.data.machines?.machines || []).find((candidate) => candidate.id === id);
	return machine?.label && machine.label !== id ? machine.label : id;
}

/**
 * Each day's cost split by the chosen dimension, bottom to top in rank order, with the tail folded into
 * "Other". `stack: 'none'` is one series and needs no legend.
 */
function stackModel(days, dimension) {
	const legend = new Map();
	const perDay = days.map((day) => {
		const parts = new Map();
		for (const cell of day.cells) {
			const name = dimension === 'none' ? '' : PICK[dimension](cell) || 'unknown';
			const slot = dimension === 'none' ? 0 : slotOf(dimension, name);
			const key = dimension === 'none' ? '' : slot < SERIES_SLOTS ? name : 'Other';
			const part = parts.get(key) || {
				key,
				slot,
				label: dimension === 'none' ? 'Cost' : slot < SERIES_SLOTS ? displayName(dimension, name) : 'Other',
				cost: 0,
			};
			part.cost += cell.cost || 0;
			parts.set(key, part);
			legend.set(key, { key, slot, label: part.label });
		}
		return {
			date: day.date,
			totals: day.totals,
			parts: [...parts.values()].sort((left, right) => left.slot - right.slot),
		};
	});
	return {
		dimension,
		perDay,
		legend: [...legend.values()].sort((left, right) => left.slot - right.slot),
	};
}

/* ------------------------------------------------------------------ periods */

/** ISO week of a local date, as `2026-W41`. */
function isoWeek(date) {
	const [year, month, day] = date.split('-').map(Number);
	const target = new Date(Date.UTC(year, month - 1, day));
	const weekday = target.getUTCDay() || 7;
	target.setUTCDate(target.getUTCDate() + 4 - weekday);
	const first = new Date(Date.UTC(target.getUTCFullYear(), 0, 1));
	const week = Math.ceil(((target - first) / 86400000 + 1) / 7);
	return `${target.getUTCFullYear()}-W${String(week).padStart(2, '0')}`;
}

/** Weeks or months rebuilt from the days on screen, for when a filter makes the stored rollups too wide. */
function periodsFromDays(days, label) {
	const periods = new Map();
	for (const day of days) {
		const key = label(day.date);
		const totals = periods.get(key) || emptyTotals();
		for (const cell of day.cells) addCell(totals, cell);
		periods.set(key, totals);
	}
	return [...periods.entries()]
		.sort((left, right) => left[0].localeCompare(right[0]))
		.map(([period, totals]) => ({ period, totals }));
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
 *
 * Sorting rebuilds the header row, which drops focus to the body; the new
 * header is refocused so a keyboard reader can press Enter twice to reverse a
 * column rather than having to tab back to it.
 */
function sortableTable(headers, rows) {
	const state = { column: null, descending: true };
	const container = el('div', { class: 'table-scroll sortable' });

	const render = (focusColumn) => {
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
					render(column);
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
		if (focusColumn != null) {
			head.children[focusColumn]?.focus();
		}
	};

	render();
	return container;
}

/**
 * A fixed table, wrapped so its own columns scroll instead of widening the
 * page: the cells do not wrap, and a narrow phone would otherwise stretch
 * every sibling — the chart included — to the widest row.
 *
 * With `onPick`, a row is a button: clicking it (or Enter) calls `onPick(index)`, which is how a
 * breakdown becomes a filter. `picked` is the index of the row that is already the filter.
 */
function table(headers, rows, { onPick = null, picked = -1 } = {}) {
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
		rows.map((row, index) => {
			const attrs = onPick
				? {
						class: `pickable${index === picked ? ' picked' : ''}`,
						role: 'button',
						tabindex: '0',
						'aria-pressed': index === picked ? 'true' : 'false',
					}
				: {};
			const tr = el(
				'tr',
				attrs,
				row.map((cell) => el('td', {}, cell)),
			);
			if (onPick) {
				tr.addEventListener('click', () => onPick(index));
				tr.addEventListener('keydown', (event) => {
					if (event.key === 'Enter' || event.key === ' ') {
						event.preventDefault();
						onPick(index);
					}
				});
			}
			return tr;
		}),
	);
	return el('div', { class: 'table-scroll' }, el('table', {}, [head, body]));
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

const CHART = { minWidth: 280, height: 240, left: 56, right: 8, top: 10, bottom: 28, maxBar: 24, gap: 2, radius: 4 };

/**
 * One bar segment: square at the baseline end, rounded where the data ends. Only the top segment of a
 * stack rounds, since that is the end of the data; the others butt against their neighbours.
 */
function segmentPath(x, top, width, height, roundTop) {
	const radius = roundTop ? Math.min(CHART.radius, width / 2, height) : 0;
	const bottom = top + height;
	const [x0, x1] = [x, x + width].map((value) => value.toFixed(1));
	if (radius === 0) return `M${x0} ${bottom.toFixed(1)}V${top.toFixed(1)}H${x1}V${bottom.toFixed(1)}Z`;
	const r = radius.toFixed(1);
	return (
		`M${x0} ${bottom.toFixed(1)}V${(top + radius).toFixed(1)}A${r} ${r} 0 0 1 ${(x + radius).toFixed(1)} ${top.toFixed(1)}` +
		`H${(x + width - radius).toFixed(1)}A${r} ${r} 0 0 1 ${x1} ${(top + radius).toFixed(1)}V${bottom.toFixed(1)}Z`
	);
}

/**
 * The daily chart, drawn as SVG with both axes, one stacked bar per day.
 *
 * A bar chart without a scale invites the reader to guess it, and every guess
 * is wrong by whatever the peak happens to be. The viewBox is measured in CSS
 * pixels of the space the chart actually has, so a phone gets a shorter chart
 * rather than the same chart shrunk until its labels are unreadable.
 *
 * Each day has a hit area as wide as its slot, not only the painted bar, which is thin. It carries
 * the tooltip on hover and on keyboard focus alike.
 */
function series(model, width, tip) {
	const { height, left, right, top, bottom, maxBar, gap } = CHART;
	const days = model.perDay;
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
		svg('text', { class: 'tick y', x: left - 8, y: (y(tick) + 4).toFixed(1) }, axisMoney(tick)),
	]);

	const slot = plotWidth / Math.max(days.length, 1);
	const barWidth = Math.max(1, Math.min(maxBar, slot * 0.7));
	const groups = days.map((day, index) => {
		const x = left + slot * (index + 0.5) - barWidth / 2;
		let floor = top + plotHeight;
		const total = day.parts.reduce((sum, part) => sum + part.cost, 0);
		const segments = day.parts
			.filter((part) => part.cost > 0)
			.map((part, position, visible) => {
				const span = total > 0 ? (part.cost / total) * (top + plotHeight - y(day.totals.cost)) : 0;
				const isTop = position === visible.length - 1;
				// A 2px gap of surface between segments, taken from each shared edge.
				const segmentTop = floor - span + (isTop ? 0 : gap / 2);
				const segmentHeight = Math.max(1, span - (position === 0 ? 0 : gap / 2) - (isTop ? 0 : gap / 2));
				floor -= span;
				return svg('path', {
					class: 'seg',
					d: segmentPath(x, segmentTop, barWidth, segmentHeight, isTop),
					style: `fill:${slotColor(part.slot)}`,
				});
			});
		const hit = svg('rect', {
			class: 'hit',
			x: (left + slot * index).toFixed(1),
			y: top,
			width: slot.toFixed(1),
			height: plotHeight,
			tabindex: '0',
			'aria-label': `${day.date}: ${money(day.totals.cost)}`,
		});
		const group = svg('g', { class: 'day' }, [...segments, hit]);
		const show = () => {
			group.classList.add('hot');
			tip.show(day, left + slot * (index + 0.5), width);
		};
		const hide = () => {
			group.classList.remove('hot');
			tip.hide();
		};
		hit.addEventListener('pointerenter', show);
		hit.addEventListener('pointerleave', hide);
		hit.addEventListener('focus', show);
		hit.addEventListener('blur', hide);
		return group;
	});

	// Roughly one label per 90px, so a year of days does not become a smear.
	const every = Math.max(1, Math.ceil(days.length / Math.max(1, Math.floor(plotWidth / 90))));
	const dates = days
		.map((day, index) => ({ day, index }))
		.filter(({ index }) => index % every === 0 || index === days.length - 1)
		.map(({ day, index }) =>
			svg('text', { class: 'tick x', x: (left + slot * (index + 0.5)).toFixed(1), y: height - 8 }, day.date.slice(5)),
		);

	const axes = [
		svg('line', { class: 'axis', x1: left, x2: left, y1: top, y2: top + plotHeight }),
		svg('line', { class: 'axis', x1: left, x2: width - right, y1: top + plotHeight, y2: top + plotHeight }),
	];

	const stackNote =
		model.dimension === 'none' ? '' : `, stacked by ${model.dimension}: ${model.legend.map((entry) => entry.label).join(', ')}`;
	return svg(
		'svg',
		{
			viewBox: `0 0 ${width} ${height}`,
			role: 'group',
			'aria-label': `Daily spend, ${days.length} day(s), peak ${money(peak)}${stackNote}`,
		},
		[...gridlines, ...groups, ...dates, ...axes],
	);
}

/**
 * One floating readout for every series on a day: the value leads, the name follows, and each row is
 * keyed by a short stroke of its colour rather than a box. Names are data, so they go in as text.
 */
function chartTip(host) {
	const node = el('div', { class: 'tip', hidden: '', role: 'status' });
	const row = (key, value, name) =>
		el('div', { class: 'tip-row' }, [
			el('span', { class: 'tip-key', style: key ? `background:${key}` : 'visibility:hidden' }),
			el('strong', {}, value),
			el('span', { class: 'muted' }, name),
		]);
	return {
		node,
		show(day, centre, width) {
			const rows = [el('div', { class: 'tip-date' }, day.date)];
			const stacked = day.parts.length > 1 || day.parts[0]?.key !== '';
			if (stacked) {
				rows.push(row(null, money(day.totals.cost), 'total'));
				for (const part of [...day.parts].sort((left, right) => right.cost - left.cost)) {
					if (part.cost > 0) rows.push(row(slotColor(part.slot), money(part.cost), part.label));
				}
			} else {
				rows.push(row(null, money(day.totals.cost), 'cost'));
			}
			rows.push(row(null, tokens(totalTokens(day.totals)), 'tokens'));
			rows.push(row(null, day.totals.messages.toLocaleString(), 'messages'));
			node.replaceChildren(...rows);
			node.hidden = false;
			const half = node.offsetWidth / 2;
			node.style.left = `${Math.max(0, Math.min(width - node.offsetWidth, centre - half)).toFixed(0)}px`;
			void host;
		},
		hide() {
			node.hidden = true;
		},
	};
}

let chartWidth = 0;

/**
 * Redraws the chart at the width its panel currently offers, in CSS pixels, so
 * the drawing is never scaled and the tick text keeps the size it asks for.
 */
function drawSeries() {
	const host = document.getElementById('series');
	const width = Math.round(Math.max(CHART.minWidth, host.clientWidth || CHART.minWidth));
	if (width === chartWidth && host.firstChild) return;
	chartWidth = width;
	if (state.chartTable) {
		host.replaceChildren(chartTable(state.stack));
		return;
	}
	const tip = chartTip(host);
	host.replaceChildren(series(state.stack, width, tip), tip.node);
}

/** The same numbers as the chart, as a table: the way to read exact values without a pointer or the colours. */
function chartTable(model) {
	const stacked = model.dimension !== 'none';
	const headers = stacked
		? ['Date', ...model.legend.map((entry) => entry.label), 'Total']
		: ['Date', 'Cost', 'Tokens', 'Messages'];
	const rows = model.perDay
		.slice()
		.reverse()
		.map((day) =>
			stacked
				? [
						day.date,
						...model.legend.map((entry) => {
							const part = day.parts.find((candidate) => candidate.key === entry.key);
							return part ? money(part.cost) : '';
						}),
						money(day.totals.cost),
					]
				: [day.date, money(day.totals.cost), tokens(totalTokens(day.totals)), day.totals.messages.toLocaleString()],
		);
	return table(headers, rows);
}

/** What the colours mean. A legend entry is also a filter: clicking one narrows everything to it. */
function drawLegend(model) {
	const host = document.getElementById('legend');
	if (model.dimension === 'none' || model.legend.length < 2) {
		host.replaceChildren();
		host.hidden = true;
		return;
	}
	host.hidden = false;
	host.replaceChildren(
		...model.legend.map((entry) => {
			const picked = state.filters[model.dimension] === entry.key;
			const item = el(
				'button',
				{ type: 'button', class: `legend-item${picked ? ' picked' : ''}`, 'aria-pressed': picked ? 'true' : 'false' },
				[el('span', { class: 'swatch', style: `background:${slotColor(entry.slot)}` }), entry.label],
			);
			if (entry.key !== 'Other') item.addEventListener('click', () => toggleFilter(model.dimension, entry.key));
			else item.disabled = true;
			return item;
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

function ago(iso, now = Date.now()) {
	const elapsed = Math.max(0, now - Date.parse(iso));
	const minutes = Math.floor(elapsed / 60000);
	if (minutes < 1) return 'just now';
	if (minutes < 60) return `${minutes}m ago`;
	const hours = Math.floor(minutes / 60);
	if (hours < 48) return `${hours}h ago`;
	return `${Math.floor(hours / 24)}d ago`;
}

/**
 * One row per machine: its label, and under it whether it is still syncing and when it last did. A
 * sandbox that has been removed never says so, so "retired" is only a statement that it has been
 * quiet for a while. Every machine on the roster is listed, even one whose usage is all counted
 * elsewhere or that has not synced yet. Without the machine list (a published page) the rows are
 * the plain ids. A row is a filter: clicking it narrows the page to that machine.
 */
function machineTable(entries, machines, columns) {
	const known = new Map((machines?.machines || []).map((machine) => [machine.id, machine]));
	const counted = new Map(entries);
	const ids = [...entries.map(([id]) => id), ...[...known.keys()].filter((id) => !counted.has(id))];
	const options = {
		onPick: (index) => toggleFilter('machine', ids[index]),
		picked: ids.indexOf(state.filters.machine),
	};
	if (known.size === 0) return table(['Machine', ...columns.slice(1)], totalsRows(entries), options);
	const rows = ids.map((id) => {
		const machine = known.get(id);
		const name = machine?.label && machine.label !== id ? machine.label : id;
		const status = !machine
			? ''
			: machine.retired
				? `retired, last synced ${ago(machine.lastSyncAt)}`
				: machine.lastSyncAt
					? `active, synced ${ago(machine.lastSyncAt)}`
					: 'not synced yet';
		const cell = el('div', { title: id }, [name, status ? el('div', { class: 'muted' }, status) : '']);
		return [cell, ...totalsRows([[id, counted.get(id) || emptyTotals()]])[0].slice(1)];
	});
	return table(['Machine', ...columns.slice(1)], rows, options);
}

/** A breakdown whose rows narrow the page to the one clicked. */
function pickableTable(label, entries, columns, key) {
	return table([label, ...columns.slice(1)], totalsRows(entries), {
		onPick: (index) => toggleFilter(key, entries[index][0]),
		picked: entries.findIndex(([name]) => name === state.filters[key]),
	});
}

/* ------------------------------------------------------------- filter bar */

const localDate = (milliseconds) =>
	new Intl.DateTimeFormat('en-CA', {
		timeZone: state.timezone,
		year: 'numeric',
		month: '2-digit',
		day: '2-digit',
	}).format(new Date(milliseconds));

function shiftDate(date, days) {
	const [year, month, day] = date.split('-').map(Number);
	const shifted = new Date(Date.UTC(year, month - 1, day + days));
	return shifted.toISOString().slice(0, 10);
}

/**
 * One row above everything it scopes: a date range first (presets before a custom range), then the
 * dimensions. Options come from all the data, not the filtered slice, so a choice can always be undone.
 */
function drawFilters(allDays) {
	const f = state.filters;
	const latest = allDays.at(-1)?.date;
	const choose = (key, label, entries) => {
		const select = el('select', { 'aria-label': label }, [
			el('option', { value: '' }, `All ${label.toLowerCase()}s`),
			...entries.map(([value, text]) => el('option', { value }, text)),
		]);
		select.value = entries.some(([value]) => value === f[key]) ? f[key] : '';
		select.addEventListener('change', () => setFilters({ [key]: select.value }));
		return el('label', { class: 'filter' }, [label, select]);
	};
	const names = (dimension) => state.rankings[dimension].map((id) => [id, displayName(dimension, id)]);

	const presets = [
		['7 days', 7],
		['30 days', 30],
		['90 days', 90],
	].map(([text, count]) => {
		const from = latest ? shiftDate(latest, -(count - 1)) : '';
		const picked = f.from === from && !f.to;
		const button = el('button', { type: 'button', class: `preset${picked ? ' picked' : ''}`, 'aria-pressed': String(picked) }, text);
		button.addEventListener('click', () => setFilters({ from: picked ? '' : from, to: '' }));
		return button;
	});
	const allTime = el(
		'button',
		{ type: 'button', class: `preset${!f.from && !f.to ? ' picked' : ''}`, 'aria-pressed': String(!f.from && !f.to) },
		'All time',
	);
	allTime.addEventListener('click', () => setFilters({ from: '', to: '' }));

	const date = (key, label) => {
		const input = el('input', { type: 'date', 'aria-label': label, value: f[key] });
		input.addEventListener('change', () => setFilters({ [key]: input.value }));
		return el('label', { class: 'filter' }, [label, input]);
	};

	const chips = [];
	if (f.project) {
		const chip = el('button', { type: 'button', class: 'chip' }, `Project: ${projectLabel(f.project).text}  ✕`);
		chip.addEventListener('click', () => setFilters({ project: '' }));
		chips.push(chip);
	}
	const reset = el('button', { type: 'button', class: 'reset' }, 'Reset');
	reset.hidden = !isFiltered();
	reset.addEventListener('click', () =>
		setFilters({ machine: '', agent: '', model: '', project: '', from: '', to: '' }),
	);

	document.getElementById('filters').replaceChildren(
		el('div', { class: 'presets', role: 'group', 'aria-label': 'Date range' }, [...presets, allTime]),
		date('from', 'From'),
		date('to', 'To'),
		choose('machine', 'Sandbox', names('machine')),
		choose('agent', 'Agent', names('agent')),
		choose('model', 'Model', names('model')),
		...chips,
		reset,
	);
}

function drawChartControls() {
	const stack = el('select', { 'aria-label': 'Stack the chart by' }, [
		el('option', { value: 'none' }, 'No stacking'),
		el('option', { value: 'machine' }, 'By sandbox'),
		el('option', { value: 'agent' }, 'By agent'),
		el('option', { value: 'model' }, 'By model'),
	]);
	stack.value = state.filters.stack;
	stack.addEventListener('change', () => setFilters({ stack: stack.value }));
	const toggle = el(
		'button',
		{ type: 'button', class: `preset${state.chartTable ? ' picked' : ''}`, 'aria-pressed': String(state.chartTable) },
		state.chartTable ? 'Show chart' : 'Show as table',
	);
	toggle.addEventListener('click', () => {
		state.chartTable = !state.chartTable;
		chartWidth = 0;
		render();
	});
	document.getElementById('chart-controls').replaceChildren(
		el('label', { class: 'filter' }, ['Stack', stack]),
		toggle,
	);
}

/* --------------------------------------------------------- projects, sessions */

/** Projects arrive as paths, or as `sha256:` hashes when ccusage was told to hide them. */
function projectLabel(project) {
	if (!project) return { text: '(no project)', title: 'The session recorded no project' };
	if (project.startsWith('sha256:')) {
		return { text: `hashed ${project.slice(7, 15)}`, title: `${project} (the path is hashed before it is synced)` };
	}
	const parts = project.split(/[\\/]/).filter(Boolean);
	return { text: parts.slice(-2).join('/') || project, title: project };
}

/** Sessions that match the sandbox, agent and date range. The model filter cannot apply: sessions carry cost only. */
function visibleSessions() {
	const { machine, agent, from, to } = state.filters;
	return (state.data.sessions?.sessions || []).filter((session) => {
		const day = localDate(session.last);
		return (
			(!machine || session.machine === machine) &&
			(!agent || session.agent === agent) &&
			(!from || day >= from) &&
			(!to || day <= to)
		);
	});
}

function drawSessions() {
	const panel = document.getElementById('sessions-panel');
	const projectsPanel = document.getElementById('projects-panel');
	if (!state.data.sessions) {
		panel.hidden = true;
		projectsPanel.hidden = true;
		return;
	}
	panel.hidden = false;
	projectsPanel.hidden = false;
	const scoped = visibleSessions();

	const byProject = new Map();
	for (const session of scoped) {
		const key = session.project || '';
		const entry = byProject.get(key) || { cost: 0, sessions: 0 };
		entry.cost += session.cost;
		entry.sessions += 1;
		byProject.set(key, entry);
	}
	const projects = [...byProject.entries()].sort((left, right) => right[1].cost - left[1].cost).slice(0, 25);
	document.getElementById('projects').replaceChildren(
		table(
			['Project', 'Cost', 'Sessions'],
			projects.map(([project, entry]) => {
				const label = projectLabel(project);
				return [el('span', { title: label.title }, label.text), money(entry.cost), entry.sessions.toLocaleString()];
			}),
			{
				onPick: (index) => toggleFilter('project', projects[index][0]),
				picked: projects.findIndex(([project]) => project === state.filters.project),
			},
		),
	);

	const shown = scoped.filter((session) => !state.filters.project || (session.project || '') === state.filters.project);
	const rows = shown.slice(0, 100).map((session) => {
		const label = projectLabel(session.project);
		return [
			{ sort: label.text, node: el('span', { title: label.title }, label.text) },
			{ sort: displayName('machine', session.machine) },
			{ sort: session.agent },
			{ sort: session.last, label: new Date(session.last).toLocaleString() },
			{ sort: session.days, label: String(session.days) },
			{ sort: session.cost, label: money(session.cost) },
		];
	});
	document.getElementById('sessions').replaceChildren(
		rows.length > 0
			? sortableTable(['Project', 'Sandbox', 'Agent', 'Last activity', 'Days', 'Cost'], rows)
			: el('p', { class: 'muted' }, 'No sessions match these filters.'),
	);
	const notes = [`Showing ${rows.length.toLocaleString()} of ${shown.length.toLocaleString()} sessions.`];
	if (state.data.sessions.truncated) notes.push(`Only the newest ${state.data.sessions.sessions.length.toLocaleString()} of ${state.data.sessions.total.toLocaleString()} are loaded.`);
	if (state.filters.model) notes.push('The model filter does not apply here: sessions carry cost, not models.');
	notes.push('Sessions carry cost only; token counts are kept per model and time, not per session.');
	document.getElementById('sessions-note').textContent = notes.join(' ');
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
	const { daily, weekly, monthly, models, machines, pricing, equivalence, expiresAt } = state.data;
	const allDays = localDays(daily, state.timezone);
	// Colours and filter options come from everything held, so a filter never repaints or hides a choice.
	state.rankings = rankings(allDays.flatMap((day) => day.cells));
	const days = applyFilters(allDays);
	const cells = days.flatMap((day) => day.cells);
	const totals = cells.reduce((accumulated, cell) => addCell(accumulated, cell), emptyTotals());

	state.days = days;
	state.stack = stackModel(days, state.filters.stack);
	drawFilters(allDays);
	drawChartControls();
	document.getElementById('totals').replaceChildren(...totalsTiles(totals, days.length));
	chartWidth = 0;
	drawLegend(state.stack);
	drawSeries();

	const modelTotals = Object.fromEntries(groupBy(cells, (cell) => cell.m));
	const columns = ['', 'Cost', 'Tokens', 'Messages'];
	document
		.getElementById('models')
		.replaceChildren(pickableTable('Model', Object.entries(modelTotals), columns, 'model'));
	document
		.getElementById('machines')
		.replaceChildren(machineTable(groupBy(cells, (cell) => cell.machine), machines, columns));
	document
		.getElementById('agents')
		.replaceChildren(pickableTable('Agent', groupBy(cells, (cell) => cell.agent), columns, 'agent'));

	// Stored weeks and months cover everything, so with a filter on they are rebuilt from the days shown.
	const periods = (rollup, label) =>
		table(
			['Period', 'Cost', 'Tokens', 'Messages'],
			(isFiltered() ? periodsFromDays(days, label) : rollup?.periods || []).map((period) => [
				period.period,
				money(period.totals.cost),
				tokens(totalTokens(period.totals)),
				period.totals.messages.toLocaleString(),
			]),
		);
	document.getElementById('weekly').replaceChildren(periods(weekly, isoWeek));
	document.getElementById('monthly').replaceChildren(periods(monthly, (date) => date.slice(0, 7)));
	drawSessions();

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

const THEME_KEY = 'ccusage.theme';
const THEMES = ['system', 'light', 'dark'];

/**
 * Applies a theme by attribute, or removes it to follow the operating system.
 *
 * The stylesheet, not this function, knows what each theme looks like: light
 * lives under `:root[data-theme='light']` and system under no attribute at all,
 * so following the OS needs no media query listener here.
 */
function applyTheme(theme) {
	if (theme === 'system') document.documentElement.removeAttribute('data-theme');
	else document.documentElement.setAttribute('data-theme', theme);
}

/**
 * Storage is unavailable in a private window and in a sandboxed frame, where
 * touching it throws. A dashboard that will not load because it could not
 * remember a colour would be a poor trade, so both ends are guarded.
 */
function storedTheme() {
	try {
		const stored = localStorage.getItem(THEME_KEY);
		return THEMES.includes(stored) ? stored : 'system';
	} catch {
		return 'system';
	}
}

function rememberTheme(theme) {
	try {
		localStorage.setItem(THEME_KEY, theme);
	} catch {
		// Not remembering it is the whole cost.
	}
}

function themes() {
	const select = document.getElementById('theme');
	const theme = storedTheme();
	applyTheme(theme);
	select.value = theme;
	select.addEventListener('change', () => {
		applyTheme(select.value);
		rememberTheme(select.value);
	});
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
	readFilters();
	themes();
	timezones();
	// The redraw changes the box being observed, which inside the callback is
	// what raises "ResizeObserver loop completed with undelivered
	// notifications"; deferring it to the next frame keeps the two apart.
	let queued = false;
	new ResizeObserver(() => {
		if (!state.days || queued) return;
		queued = true;
		requestAnimationFrame(() => {
			queued = false;
			drawSeries();
		});
	}).observe(document.getElementById('series'));
	try {
		state.data = await load();
		render();
	} catch (error) {
		document.getElementById('gate-detail').textContent = error.message;
	}
}

void main();
