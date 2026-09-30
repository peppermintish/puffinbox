'use strict';

const assert = require('node:assert/strict');
const {
  activeAccountMatches,
  nextActiveAccountRecord,
  nextPackageEpoch,
  packageTransferMatches,
  packageTransferCanBeRemoved,
  settleLogout,
  localSignOutApplies,
  offlineStorageErrorCode,
} = require('../web/offline-cache.js');

let tokenNumber = 0;
const makeToken = () => `token-${++tokenNumber}`;

const firstAliceTab = nextActiveAccountRecord(null, 'alice', null, makeToken);
assert.equal(firstAliceTab.accountId, 'alice');
assert.equal(activeAccountMatches(firstAliceTab, 'alice', firstAliceTab.generation), true);

const secondAliceTab = nextActiveAccountRecord(firstAliceTab, 'alice', null, makeToken);
assert.equal(secondAliceTab.generation, firstAliceTab.generation,
  'two tabs opening the same account share the stored generation');

const oldTransfer = {
  accountId: 'alice', packageId: 'movie-1', transferToken: 'transfer-old', status: 'downloading',
};
assert.equal(packageTransferMatches(firstAliceTab, oldTransfer, 'alice', firstAliceTab.generation, 'transfer-old'), true);

const initialPackageEpoch = nextPackageEpoch(null, null, () => 'epoch-before-remove');
const tombstonedPackageEpoch = nextPackageEpoch(initialPackageEpoch, 'epoch-before-remove', () => 'epoch-after-remove');
assert.throws(() => nextPackageEpoch(tombstonedPackageEpoch, 'epoch-before-remove', makeToken),
  /removed or replaced/i, 'a transfer that has not reached its first write cannot start after another tab removes its package');
const explicitNewSaveEpoch = nextPackageEpoch(tombstonedPackageEpoch, 'epoch-after-remove', () => 'epoch-for-new-save');
assert.equal(explicitNewSaveEpoch.value, 'epoch-for-new-save',
  'an explicit later save can proceed using the refreshed package generation');
assert.throws(() => nextPackageEpoch(explicitNewSaveEpoch, 'epoch-after-remove', makeToken),
  /removed or replaced/i, 'a delayed replacement cannot bless a newer competing save');

const removedPackage = null;
assert.equal(packageTransferMatches(firstAliceTab, removedPackage, 'alice', firstAliceTab.generation, 'transfer-old'), false,
  'a transfer in another tab cannot recreate a package after its row is removed');

const replacementTransfer = { ...oldTransfer, transferToken: 'transfer-new', status: 'downloading' };
assert.equal(packageTransferMatches(firstAliceTab, replacementTransfer, 'alice', firstAliceTab.generation, 'transfer-old'), false,
  'a late write from a removed transfer cannot overwrite an explicit replacement');
assert.equal(packageTransferMatches(firstAliceTab, replacementTransfer, 'alice', firstAliceTab.generation, 'transfer-new'), true);
assert.equal(packageTransferCanBeRemoved(replacementTransfer, 'transfer-old'), false,
  'a stale remove action cannot delete a package replaced from another tab');
assert.equal(packageTransferCanBeRemoved(replacementTransfer, 'transfer-new'), true);

const bobTab = nextActiveAccountRecord(firstAliceTab, 'bob', firstAliceTab.generation, makeToken);
assert.equal(packageTransferMatches(bobTab, oldTransfer, 'alice', firstAliceTab.generation, 'transfer-old'), false,
  'writes and offline stream reads from the prior account stop after another tab switches accounts');
assert.throws(() => nextActiveAccountRecord(bobTab, 'alice', firstAliceTab.generation, makeToken),
  /offline account changed/i, 'a stale tab cannot switch the shared account back using an old generation');
assert.throws(() => nextActiveAccountRecord(bobTab, null, firstAliceTab.generation, makeToken),
  /offline account changed/i, 'a delayed logout cannot clear a newer account session');

const loggedOut = nextActiveAccountRecord(bobTab, null, bobTab.generation, makeToken);
assert.equal(activeAccountMatches(loggedOut, 'bob', bobTab.generation), false,
  'logout rotates the shared generation and makes open streams fail their next chunk guard');
const aliceAgain = nextActiveAccountRecord(loggedOut, 'alice', loggedOut.generation, makeToken);
assert.notEqual(aliceAgain.generation, firstAliceTab.generation,
  'logging back in after logout does not revive old in-flight transfers');
assert.equal(packageTransferMatches(aliceAgain, replacementTransfer, 'alice', firstAliceTab.generation, 'transfer-new'), false);
const delayedLogoutMarker = JSON.stringify({ accountId: 'alice', generation: firstAliceTab.generation });
assert.equal(localSignOutApplies(delayedLogoutMarker, bobTab), false,
  'a stale local sign-out marker does not suppress a newer account session in another tab');
assert.equal(localSignOutApplies(delayedLogoutMarker, firstAliceTab), true,
  'a failed server logout still requires local login while its original account session remains stored');
assert.equal(localSignOutApplies(delayedLogoutMarker, null), true,
  'local sign-out remains effective when browser storage is temporarily unavailable');
assert.equal(offlineStorageErrorCode({ name: 'QuotaExceededError', message: 'Failed to execute put' }), 'storage-quota',
  'browser quota exceptions are identified even when their message omits quota details');
assert.equal(offlineStorageErrorCode({ name: 'DOMException', code: 22, message: 'Operation failed' }), 'storage-quota',
  'legacy quota exception codes map to the storage quota state');
assert.equal(offlineStorageErrorCode(new Error('The quota has been exceeded.')), 'storage-quota',
  'quota failures with browser-specific names are identified from their message');
assert.equal(offlineStorageErrorCode({ name: 'SecurityError', message: 'IndexedDB denied' }), 'storage-unavailable',
  'storage permission failures remain distinct from interrupted network transfers');
assert.equal(offlineStorageErrorCode(new Error('Network request failed')), null,
  'ordinary network interruptions are not mislabeled as storage failures');

async function logoutStillClearsLocalStateWhenServerIsOffline() {
  let localClearAttempts = 0;
  const outcome = await settleLogout(
    async () => { throw new Error('server unreachable'); },
    async () => { localClearAttempts += 1; },
  );
  assert.equal(outcome.serverRevoked, false);
  assert.equal(outcome.localCopiesCleared, true);
  assert.equal(localClearAttempts, 1, 'server failure does not skip the local account tombstone and cache removal');

  const staleLogout = await settleLogout(
    async () => {},
    async () => { throw new Error('active account changed'); },
  );
  assert.equal(staleLogout.serverRevoked, true);
  assert.equal(staleLogout.localCopiesCleared, false,
    'a delayed logout reports that it left a newer tab account untouched');
}

logoutStillClearsLocalStateWhenServerIsOffline().then(() => {
  process.stdout.write('Offline cache account, transfer-generation, and logout-helper tests passed.\n');
}).catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
