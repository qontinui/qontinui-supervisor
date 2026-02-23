import React from 'react';

type ActionState = string | null;

export function SmallBtn({
  label,
  activeLabel,
  onClick,
  busy,
  busyKey,
  variant,
  disabled,
}: {
  label: string;
  activeLabel: string;
  onClick: () => void;
  busy: ActionState;
  busyKey?: string;
  variant?: 'danger' | 'warning';
  disabled?: boolean;
}) {
  const isActive = busy === (busyKey ?? label);
  const style: React.CSSProperties = {
    padding: '0.2rem 0.5rem',
    fontSize: '0.75rem',
  };
  if (variant === 'danger') {
    style.borderColor = 'var(--danger)';
    style.color = 'var(--danger)';
  } else if (variant === 'warning') {
    style.borderColor = 'var(--warning)';
    style.color = 'var(--warning)';
  }
  return (
    <button className="btn" style={style} disabled={busy !== null || disabled} onClick={onClick}>
      {isActive ? activeLabel : label}
    </button>
  );
}
