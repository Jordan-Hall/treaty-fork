import { counter, bump } from './counter';
import * as util from './util';
import dflt from './defaults';

// Live bindings across the bundle: `counter` is read after `bump()` changed it.
export { label } from './labels';
export const report = () => [counter, util.double(counter), dflt];

console.log(JSON.stringify(report()));
bump();
console.log(JSON.stringify(report()));
import('./lazy').then((m) => console.log('lazy', m.name));
