// Unit checks for frontend/src/order.ts. Zero dependencies (Node strips the types).
import assert from "node:assert/strict";
import { moveItem, shapeMismatch } from "../frontend/src/order.ts";

const mv = (a, f, t) => (moveItem(a, f, t), a);
assert.deepEqual(mv([1, 2, 3, 4], 3, 1), [1, 4, 2, 3]); // up
assert.deepEqual(mv([1, 2, 3, 4], 0, 2), [2, 3, 1, 4]); // down
assert.deepEqual(mv([1, 2, 3, 4], 2, 2), [1, 2, 3, 4]); // same index
assert.deepEqual(mv([1, 2, 3, 4], 3, 0), [4, 1, 2, 3]); // to first
assert.deepEqual(mv([1, 2, 3, 4], 0, 3), [2, 3, 4, 1]); // to last

const s = (audio, subs) => ({ audio, subs });
assert.deepEqual(shapeMismatch([[s(2, 1), s(2, 1)]]), [[false, false]]);
assert.deepEqual(shapeMismatch([[s(2, 1), s(1, 1), s(2, 1)]]), [[false, true, false]]);
assert.deepEqual(shapeMismatch([[s(2, 1), s(2, 0)]]), [[false, true]]);
assert.deepEqual(shapeMismatch([[s(1, 0)], [s(3, 2)], [s(2, 1), s(2, 1)]]), [[false], [false], [false, false]]);
console.log("check-order: ok");
