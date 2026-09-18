/**
 * Lints the dashboard page the binary serves.
 *
 * The bundle has no build step: `src/lib.rs` embeds these files with
 * `include_str!`, and `ccusage sync dashboard` hands the same bytes to the
 * browser whether it is hosting locally or deploying. That makes the file on
 * disk the output, so linting it here lints what ships — but only as long as
 * every embedded HTML file is covered, which the first test checks rather than
 * assumes.
 */

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { HtmlValidate } from 'html-validate';

const crate = dirname(fileURLToPath(import.meta.url));
const assets = join(crate, 'assets');

const htmlValidate = new HtmlValidate({
	extends: ['html-validate:recommended'],
	rules: {
		// Written the way the repository's formatter writes HTML, which these
		// two rules disagree with on taste alone.
		'doctype-style': 'off',
		'void-style': 'off',
	},
});

/** The HTML files `src/lib.rs` embeds, which are the ones actually served. */
async function embeddedHtml(): Promise<string[]> {
	const source = await readFile(join(crate, 'src', 'lib.rs'), 'utf8');
	return [...source.matchAll(/include_str!\("\.\.\/assets\/([^"]+\.html)"\)/g)].map(
		(match) => match[1] as string,
	);
}

test('every HTML file the binary embeds is linted here', async () => {
	assert.deepEqual(await embeddedHtml(), ['index.html']);
});

test('the page the dashboard serves is valid HTML', async () => {
	for (const name of await embeddedHtml()) {
		const report = await htmlValidate.validateFile(join(assets, name));
		const messages = report.results.flatMap((result) =>
			result.messages.map((message) => `${name}:${message.line}:${message.column} ${message.ruleId} ${message.message}`),
		);
		assert.deepEqual(messages, [], messages.join('\n'));
	}
});

test('the page asks for the assets that ship beside it, and nothing off-site', async () => {
	const page = await readFile(join(assets, 'index.html'), 'utf8');

	const references = [...page.matchAll(/(?:href|src)="([^"]+)"/g)].map((match) => match[1] as string);

	assert.deepEqual(references, ['styles.css', 'app.js']);
});
