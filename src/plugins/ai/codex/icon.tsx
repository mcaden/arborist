interface CodexIconProps {
  readonly className?: string;
}

export function CodexIcon({ className }: CodexIconProps): JSX.Element {
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
      <path d="M8 7 3 12l5 5" />
      <path d="m16 7 5 5-5 5" />
      <path d="m14 5-4 14" />
    </svg>
  );
}
