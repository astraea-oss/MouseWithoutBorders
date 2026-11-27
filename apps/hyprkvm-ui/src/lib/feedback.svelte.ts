// The short message at the top of a page, and live actions that report
// through it. Shared so Home and Settings say things the same way.

import { asError } from './backend.svelte';

export const toast = $state({ text: '', bad: false, show: false });

let timer: ReturnType<typeof setTimeout> | undefined;

export function note(text: string, bad = false) {
  toast.text = text;
  toast.bad = bad;
  toast.show = true;
  clearTimeout(timer);
  timer = setTimeout(() => (toast.show = false), bad ? 4000 : 1800);
}

/** Runs a live action and reports a failure without throwing. */
export async function run(action: () => Promise<unknown>, done?: string) {
  try {
    await action();
    if (done) note(done);
  } catch (error) {
    note(asError(error).message, true);
  }
}

export function short(text: string, max: number) {
  return text.length > max ? text.slice(0, max - 1) + '…' : text;
}

/** Keeps the end of a path, which is the part that tells files apart. */
export function tail(text: string, max: number) {
  return text.length > max ? '…' + text.slice(text.length - max + 1) : text;
}
