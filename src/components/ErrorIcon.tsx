// Shared error/alert icon (circle with an exclamation mark) used to flag a
// surface that has a blocking validation error. Presentation-only: consumers
// control colour (via `currentColor`), sizing (via `className`), and the
// surrounding label / tooltip semantics.

interface ErrorIconProps {
  readonly className?: string;
}

export function ErrorIcon({ className }: ErrorIconProps): JSX.Element {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      className={className}
    >
      <circle cx="12" cy="12" r="10" />
      <line x1="12" y1="8" x2="12" y2="12" />
      <line x1="12" y1="16" x2="12.01" y2="16" />
    </svg>
  );
}
