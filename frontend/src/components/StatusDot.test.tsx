import { describe, it, expect } from 'vitest';
import { render } from '@testing-library/react';
import { StatusDot } from './StatusDot';

describe('StatusDot', () => {
  it('renders a green dot when up=true', () => {
    const { container } = render(<StatusDot up={true} />);
    const dot = container.querySelector('span')!;

    expect(dot).toBeInTheDocument();
    expect(dot.style.background).toBe('var(--success)');
    expect(dot.style.borderRadius).toBe('50%');
    expect(dot.style.width).toBe('8px');
    expect(dot.style.height).toBe('8px');
  });

  it('renders a red dot when up=false', () => {
    const { container } = render(<StatusDot up={false} />);
    const dot = container.querySelector('span')!;

    expect(dot.style.background).toBe('var(--danger)');
  });

  it('renders a yellow/warning dot when error=true', () => {
    const { container } = render(<StatusDot up={true} error={true} />);
    const dot = container.querySelector('span')!;

    // error takes precedence over up
    expect(dot.style.background).toBe('var(--warning)');
  });

  it('renders a yellow dot when error=true even if up=false', () => {
    const { container } = render(<StatusDot up={false} error={true} />);
    const dot = container.querySelector('span')!;

    // error takes precedence regardless of up state
    expect(dot.style.background).toBe('var(--warning)');
  });

  it('renders green when up=true and error is not provided', () => {
    const { container } = render(<StatusDot up={true} />);
    const dot = container.querySelector('span')!;

    expect(dot.style.background).toBe('var(--success)');
  });

  it('renders green when up=true and error=false', () => {
    const { container } = render(<StatusDot up={true} error={false} />);
    const dot = container.querySelector('span')!;

    expect(dot.style.background).toBe('var(--success)');
  });
});
