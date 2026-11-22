// Unit checks for frontend/src/order.ts. Zero dependencies (Node strips the types).
import assert from "node:assert/strict";
import { mergeBlock, moveItem, shapeMismatch } from "../frontend/src/order.ts";
import { speedChoices } from "../frontend/src/studio/speeds.ts";

const mv = (a, f, t) => (moveItem(a, f, t), a);
assert.deepEqual(mv([1, 2, 3, 4], 3, 1), [1, 4, 2, 3]); // up
assert.deepEqual(mv([1, 2, 3, 4], 0, 2), [2, 3, 1, 4]); // down
assert.deepEqual(mv([1, 2, 3, 4], 2, 2), [1, 2, 3, 4]); // same index
assert.deepEqual(mv([1, 2, 3, 4], 3, 0), [4, 1, 2, 3]); // to first
assert.deepEqual(mv([1, 2, 3, 4], 0, 3), [2, 3, 4, 1]); // to last

// mergeBlock: "id" + "m" (merged) per item, e.g. "2m" = id 2 merged into its predecessor.
const items = (spec) => spec.split(" ").map((x) => ({ id: Number(x[0]), mergeWithPrevious: x[1] === "m" }));
const show = (a) => a.map((t) => `${t.id}${t.mergeWithPrevious ? "m" : ""}`).join(" ");
const mb = (spec, ids) => { const a = items(spec); mergeBlock(a, new Set(ids)); return show(a); };
assert.equal(mb("1 2 3 4", [2, 3]), "1 2 3m 4"); // contiguous
assert.equal(mb("1 2 3 4", [1, 2, 3, 4]), "1 2m 3m 4m"); // merge all
assert.equal(mb("1 2 3 4", [1, 3]), "1 3m 2 4"); // gathered at the first one
assert.equal(mb("1 2 3 4m", [1, 3]), "1 3m 2 4"); // 4 was merged into 3, must not join 2
assert.equal(mb("1 2m 3 4", [2, 4]), "1 2m 4m 3"); // first keeps its own merge
assert.equal(mb("1 2 3", [2]), "1 2 3"); // fewer than two: no-op

const s = (audio, subs) => ({ audio, subs });
assert.deepEqual(shapeMismatch([[s(2, 1), s(2, 1)]]), [[false, false]]);
assert.deepEqual(shapeMismatch([[s(2, 1), s(1, 1), s(2, 1)]]), [[false, true, false]]);
assert.deepEqual(shapeMismatch([[s(2, 1), s(2, 0)]]), [[false, true]]);
assert.deepEqual(shapeMismatch([[s(1, 0)], [s(3, 2)], [s(2, 1), s(2, 1)]]), [[false], [false], [false, false]]);

assert.deepEqual(speedChoices([]), [["12", "12x"], ["8", "8x"], ["4", "4x"]]);
assert.deepEqual(speedChoices([16, 8.2, 2.4]), [["16", "16x"], ["12", "12x"], ["8", "8x"], ["4", "4x"], ["2.4", "2.4x"]]);
assert.deepEqual(speedChoices([4.1]), [["12", "12x"], ["8", "8x"], ["4", "4x"]]);

console.log("check-order: ok");
