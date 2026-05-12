// Custom-process plugin TS interface re-exports.
//
// Issue #97 intentionally leaves frontend custom-process hooks empty; these files
// mirror backend plugin layout so per-process UI hooks can be added later.

export type { CustomProcessPlugin } from '../index';
