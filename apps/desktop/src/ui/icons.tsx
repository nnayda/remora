/* Remora UI kit — typed Lucide-geometry line icons (MIT geometry).
 * Stroke 2, round caps. Inherit currentColor. Ported from
 * notes/design-system/ui_kits/remora-app/icons.js. */
import type { JSX } from "react";

export type IconProps = { size?: number } & React.SVGProps<SVGSVGElement>;

function Svg({ size = 16, children, ...rest }: IconProps): JSX.Element {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      {...rest}
    >
      {children}
    </svg>
  );
}

export function Terminal(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <polyline points="4 17 10 11 4 5" />
      <line x1={12} y1={19} x2={20} y2={19} />
    </Svg>
  );
}

export function FileCode(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
      <polyline points="14 2 14 8 20 8" />
      <polyline points="10 13 8 15 10 17" />
      <polyline points="14 13 16 15 14 17" />
    </Svg>
  );
}

export function GitBranch(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <line x1={6} y1={3} x2={6} y2={15} />
      <circle cx={18} cy={6} r={3} />
      <circle cx={6} cy={18} r={3} />
      <path d="M18 9a9 9 0 0 1-9 9" />
    </Svg>
  );
}

export function GitPullRequest(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <circle cx={18} cy={18} r={3} />
      <circle cx={6} cy={6} r={3} />
      <path d="M13 6h3a2 2 0 0 1 2 2v7" />
      <line x1={6} y1={9} x2={6} y2={21} />
    </Svg>
  );
}

export function Plus(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <line x1={12} y1={5} x2={12} y2={19} />
      <line x1={5} y1={12} x2={19} y2={12} />
    </Svg>
  );
}

export function Split(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <rect x={3} y={3} width={18} height={18} rx={2} />
      <line x1={12} y1={3} x2={12} y2={21} />
    </Svg>
  );
}

export function X(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <line x1={18} y1={6} x2={6} y2={18} />
      <line x1={6} y1={6} x2={18} y2={18} />
    </Svg>
  );
}

export function Search(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <circle cx={11} cy={11} r={8} />
      <line x1={21} y1={21} x2={16.65} y2={16.65} />
    </Svg>
  );
}

export function Settings(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <circle cx={12} cy={12} r={3} />
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
    </Svg>
  );
}

export function ChevronDown(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <polyline points="6 9 12 15 18 9" />
    </Svg>
  );
}

export function ChevronRight(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <polyline points="9 18 15 12 9 6" />
    </Svg>
  );
}

export function Folder(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
    </Svg>
  );
}

export function Server(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <rect x={2} y={2} width={20} height={8} rx={2} />
      <rect x={2} y={14} width={20} height={8} rx={2} />
      <line x1={6} y1={6} x2={6.01} y2={6} />
      <line x1={6} y1={18} x2={6.01} y2={18} />
    </Svg>
  );
}

export function Play(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <polygon points="5 3 19 12 5 21 5 3" />
    </Svg>
  );
}

export function Check(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <polyline points="20 6 9 17 4 12" />
    </Svg>
  );
}

export function Bell(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <path d="M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9" />
      <path d="M13.73 21a2 2 0 0 1-3.46 0" />
    </Svg>
  );
}

export function PanelRight(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <rect x={3} y={3} width={18} height={18} rx={2} />
      <line x1={15} y1={3} x2={15} y2={21} />
    </Svg>
  );
}

export function Sidebar(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <rect x={3} y={3} width={18} height={18} rx={2} />
      <line x1={9} y1={3} x2={9} y2={21} />
    </Svg>
  );
}

export function More(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <circle cx={12} cy={12} r={1} />
      <circle cx={19} cy={12} r={1} />
      <circle cx={5} cy={12} r={1} />
    </Svg>
  );
}

export function Command(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <path d="M15 6a3 3 0 1 0 3 3v6a3 3 0 1 0-3-3H9a3 3 0 1 0 3 3V9a3 3 0 1 0-3 3z" />
    </Svg>
  );
}

export function Cpu(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <rect x={4} y={4} width={16} height={16} rx={2} />
      <rect x={9} y={9} width={6} height={6} />
      <line x1={9} y1={1} x2={9} y2={4} />
      <line x1={15} y1={1} x2={15} y2={4} />
      <line x1={9} y1={20} x2={9} y2={23} />
      <line x1={15} y1={20} x2={15} y2={23} />
      <line x1={20} y1={9} x2={23} y2={9} />
      <line x1={20} y1={14} x2={23} y2={14} />
      <line x1={1} y1={9} x2={4} y2={9} />
      <line x1={1} y1={14} x2={4} y2={14} />
    </Svg>
  );
}

export function Activity(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <polyline points="22 12 18 12 15 21 9 3 6 12 2 12" />
    </Svg>
  );
}

export function ArrowUp(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <line x1={12} y1={19} x2={12} y2={5} />
      <polyline points="5 12 12 5 19 12" />
    </Svg>
  );
}

export function Folders(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <path d="M8 17a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3l2 2h6a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2z" />
      <path d="M2 8v11a2 2 0 0 0 2 2h14" />
    </Svg>
  );
}

export function Trash(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <polyline points="3 6 5 6 21 6" />
      <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
    </Svg>
  );
}

export function Unplug(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <path d="M19 5 22 2" />
      <path d="M2 22l3-3" />
      <path d="M6.3 14.3 9 11.6l3.4 3.4-2.7 2.7a2.4 2.4 0 0 1-3.4 0l0 0a2.4 2.4 0 0 1 0-3.4Z" />
      <path d="M14.4 9.6 17.1 6.9a2.4 2.4 0 0 1 3.4 0l0 0a2.4 2.4 0 0 1 0 3.4l-2.7 2.7Z" />
    </Svg>
  );
}

/* --- Additions the app needs (standard Lucide geometry) --- */

export function RotateCw(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <polyline points="21 2 21 8 15 8" />
      <path d="M21 8a9 9 0 1 0 2 6" />
    </Svg>
  );
}

export function AlertTriangle(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
      <line x1={12} y1={9} x2={12} y2={13} />
      <line x1={12} y1={17} x2={12.01} y2={17} />
    </Svg>
  );
}

export function Sun(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <circle cx={12} cy={12} r={4} />
      <line x1={12} y1={2} x2={12} y2={4} />
      <line x1={12} y1={20} x2={12} y2={22} />
      <line x1={4.93} y1={4.93} x2={6.34} y2={6.34} />
      <line x1={17.66} y1={17.66} x2={19.07} y2={19.07} />
      <line x1={2} y1={12} x2={4} y2={12} />
      <line x1={20} y1={12} x2={22} y2={12} />
      <line x1={4.93} y1={19.07} x2={6.34} y2={17.66} />
      <line x1={17.66} y1={6.34} x2={19.07} y2={4.93} />
    </Svg>
  );
}

export function Moon(props: IconProps): JSX.Element {
  return (
    <Svg {...props}>
      <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
    </Svg>
  );
}
