interface CopilotIconProps {
  className?: string;
}

export function CopilotIcon({ className }: CopilotIconProps): JSX.Element {
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
      <path d="M4 13c0-2 1-3 2.5-3.5C7 6 9 4 12 4s5 2 5.5 5.5C19 10 20 11 20 13v3c0 2-3 4-8 4s-8-2-8-4z" />
      <circle cx="9.5" cy="14" r="1.25" fill="currentColor" stroke="none" />
      <circle cx="14.5" cy="14" r="1.25" fill="currentColor" stroke="none" />
    </svg>
  );
}
