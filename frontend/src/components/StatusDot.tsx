export function StatusDot({ up, error }: { up: boolean; error?: boolean }) {
  const color = error ? 'var(--warning)' : up ? 'var(--success)' : 'var(--danger)';
  return (
    <span
      style={{
        display: 'inline-block',
        width: 8,
        height: 8,
        borderRadius: '50%',
        background: color,
        marginRight: 6,
      }}
    />
  );
}
