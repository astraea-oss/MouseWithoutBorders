import type { Component, Snippet } from 'svelte';

type Dot = '' | 'on' | 'live' | 'blink';

export const Card: Component<{ dots?: boolean; class?: string; children?: Snippet; [attr: string]: unknown }>;
export const Label: Component<{ class?: string; children?: Snippet }>;
export const Pill: Component<{
  variant?: '' | 'ghost' | 'danger';
  small?: boolean;
  wide?: boolean;
  disabled?: boolean;
  onclick?: (event: MouseEvent) => void;
  children?: Snippet;
  [attr: string]: unknown;
}>;
export const Segment: Component<{
  options: { value: any; label: string; disabled?: boolean }[];
  value: any;
  label: string;
  fit?: boolean;
  disabled?: boolean;
  onchange?: (value: any) => void;
}>;
export const StatusDot: Component<{ state?: Dot; label?: string }>;
export const Switch: Component<{
  checked?: boolean;
  red?: boolean;
  disabled?: boolean;
  label: string;
  onchange?: (checked: boolean) => void;
}>;
export const Tile: Component<{
  caption: string;
  word: string;
  foot?: string;
  dot?: Dot;
  fresh?: boolean;
  disabled?: boolean;
  title?: string;
  onclick?: () => void;
}>;
