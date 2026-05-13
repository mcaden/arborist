interface ClaudeIconProps {
  className?: string;
}

export function ClaudeIcon({ className }: ClaudeIconProps): JSX.Element {
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
      <circle cx="12" cy="12" r="9" />
      <path d="M15.5 8.5 A5 5 0 1 0 15.5 15.5" />
    </svg>
  );
}
