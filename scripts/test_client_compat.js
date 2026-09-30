'use strict';

const assert = require('node:assert/strict');
const { replaceChildren, scanStatusLabel, startupFailure, mediaErrorDescription } = require('../web/client-compat.js');

function makeLegacyElement(children = []) {
  return {
    children: [...children],
    get firstChild() { return this.children[0] || null; },
    appendChild(node) {
      this.children.push(node);
      return node;
    },
    removeChild(node) {
      const index = this.children.indexOf(node);
      if (index < 0) throw new Error('node was not a child');
      this.children.splice(index, 1);
      return node;
    },
  };
}

const oldChild = { label: 'old' };
const nextChildren = [{ label: 'first' }, { label: 'second' }];
const legacyGrid = makeLegacyElement([oldChild]);
replaceChildren(legacyGrid, ...nextChildren);
assert.deepEqual(legacyGrid.children, nextChildren, 'fallback clears old nodes and appends replacements');

let nativeArguments;
const modernGrid = { replaceChildren(...nodes) { nativeArguments = nodes; } };
replaceChildren(modernGrid, ...nextChildren);
assert.deepEqual(nativeArguments, nextChildren, 'native method is used when available');

assert.equal(scanStatusLabel('completed_with_errors'), 'completed with errors');
assert.equal(scanStatusLabel('unknown'), 'unknown');
assert.equal(mediaErrorDescription({ code: 4, message: 'unsupported HLS codec' }),
  'Browser media error 4 (source not supported): unsupported HLS codec.');
assert.match(mediaErrorDescription({ code: 3 }), /decode error/);

const renderError = startupFailure(new Error('grid rendering failed'), 'render');
assert.equal(renderError.title, 'Page could not be displayed');
assert.notEqual(renderError.title, 'Server unavailable');
assert.match(renderError.description, /server responded/i);
assert.equal(startupFailure(new Error('request failed'), 'server').title, 'Server unavailable');

process.stdout.write('Client compatibility tests passed.\n');
