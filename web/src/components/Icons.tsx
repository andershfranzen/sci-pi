import type { ReactNode, SVGProps } from "react";

type P = SVGProps<SVGSVGElement> & { size?: number };

function I({ size = 16, children, ...rest }: P & { children: ReactNode }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      {...rest}
    >
      {children}
    </svg>
  );
}

export const IconMenu = (p: P) => (
  <I {...p}>
    <path d="M4 7h16M4 12h16M4 17h16" />
  </I>
);
export const IconPlus = (p: P) => (
  <I {...p}>
    <path d="M12 5v14M5 12h14" />
  </I>
);
export const IconInbox = (p: P) => (
  <I {...p}>
    <path d="M22 12h-6l-2 3h-4l-2-3H2" />
    <path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z" />
  </I>
);
export const IconBell = (p: P) => (
  <I {...p}>
    <path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9" />
    <path d="M10.3 21a1.94 1.94 0 0 0 3.4 0" />
  </I>
);
export const IconBellOff = (p: P) => (
  <I {...p}>
    <path d="M8.7 3A6 6 0 0 1 18 8a21.3 21.3 0 0 0 .6 5" />
    <path d="M17 17H3s3-2 3-9a4.67 4.67 0 0 1 .3-1.7" />
    <path d="M10.3 21a1.94 1.94 0 0 0 3.4 0" />
    <path d="m2 2 20 20" />
  </I>
);
export const IconStop = (p: P) => (
  <I {...p}>
    <rect x="6" y="6" width="12" height="12" rx="2" />
  </I>
);
export const IconTrash = (p: P) => (
  <I {...p}>
    <path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6" />
  </I>
);
export const IconRefresh = (p: P) => (
  <I {...p}>
    <path d="M21 12a9 9 0 0 1-15.5 6.2L3 16" />
    <path d="M3 12a9 9 0 0 1 15.5-6.2L21 8" />
    <path d="M21 3v5h-5M3 21v-5h5" />
  </I>
);
export const IconChevron = (p: P) => (
  <I {...p}>
    <path d="m9 6 6 6-6 6" />
  </I>
);
export const IconChevronDown = (p: P) => (
  <I {...p}>
    <path d="m6 9 6 6 6-6" />
  </I>
);
export const IconFolder = (p: P) => (
  <I {...p}>
    <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
  </I>
);
export const IconBranch = (p: P) => (
  <I {...p}>
    <circle cx="6" cy="5" r="2" />
    <circle cx="6" cy="19" r="2" />
    <circle cx="18" cy="7" r="2" />
    <path d="M6 7v10M18 9a6 6 0 0 1-6 6H6" />
  </I>
);
export const IconTerminal = (p: P) => (
  <I {...p}>
    <path d="m5 8 4 4-4 4M12 17h7" />
  </I>
);
export const IconEdit = (p: P) => (
  <I {...p}>
    <path d="M12 20h9" />
    <path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z" />
  </I>
);
export const IconFile = (p: P) => (
  <I {...p}>
    <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z" />
    <path d="M14 3v6h6" />
  </I>
);
export const IconSearch = (p: P) => (
  <I {...p}>
    <circle cx="11" cy="11" r="7" />
    <path d="m20 20-3.5-3.5" />
  </I>
);
export const IconGlobe = (p: P) => (
  <I {...p}>
    <circle cx="12" cy="12" r="9" />
    <path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18" />
  </I>
);
export const IconBrain = (p: P) => (
  <I {...p}>
    <path d="M9 4a3 3 0 0 0-3 3 3 3 0 0 0-2 5 3 3 0 0 0 2 5 3 3 0 0 0 6 1V5a2 2 0 0 0-3-1z" />
    <path d="M15 4a3 3 0 0 1 3 3 3 3 0 0 1 2 5 3 3 0 0 1-2 5 3 3 0 0 1-6 1" />
  </I>
);
export const IconMove = (p: P) => (
  <I {...p}>
    <path d="M5 12h14M13 6l6 6-6 6" />
  </I>
);
export const IconTool = (p: P) => (
  <I {...p}>
    <path d="M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18l3 3 6.3-6.3a4 4 0 0 0 5.4-5.4l-2.5 2.5-2.4-.6-.6-2.4z" />
  </I>
);
export const IconX = (p: P) => (
  <I {...p}>
    <path d="M18 6 6 18M6 6l12 12" />
  </I>
);
export const IconCheck = (p: P) => (
  <I {...p}>
    <path d="m5 12 5 5L20 7" />
  </I>
);
export const IconArrowUp = (p: P) => (
  <I {...p}>
    <path d="M12 19V5M5 12l7-7 7 7" />
  </I>
);
export const IconShield = (p: P) => (
  <I {...p}>
    <path d="M12 3 4 6v6c0 5 3.5 8 8 9 4.5-1 8-4 8-9V6z" />
  </I>
);
export const IconServer = (p: P) => (
  <I {...p}>
    <rect x="3" y="4" width="18" height="7" rx="2" />
    <rect x="3" y="13" width="18" height="7" rx="2" />
    <path d="M7 7.5h.01M7 16.5h.01" />
  </I>
);
export const IconExternal = (p: P) => (
  <I {...p}>
    <path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5" />
  </I>
);
export const IconGit = (p: P) => (
  <I {...p}>
    <circle cx="12" cy="12" r="3" />
    <path d="M3 12h6M15 12h6" />
  </I>
);
export const IconHome = (p: P) => (
  <I {...p}>
    <path d="m3 11 9-7 9 7v9a1 1 0 0 1-1 1h-5v-6H9v6H4a1 1 0 0 1-1-1z" />
  </I>
);
export const IconMore = (p: P) => (
  <I {...p}>
    <circle cx="5" cy="12" r="1" />
    <circle cx="12" cy="12" r="1" />
    <circle cx="19" cy="12" r="1" />
  </I>
);

export const IconPin = (p: P) => (
  <I {...p}>
    <path d="M12 17v5M9 3h6l-1 6 4 4H6l4-4z" />
  </I>
);
export const IconArchive = (p: P) => (
  <I {...p}>
    <rect x="3" y="4" width="18" height="5" rx="1" />
    <path d="M5 9v10a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V9M10 13h4" />
  </I>
);
export const IconImage = (p: P) => (
  <I {...p}>
    <rect x="3" y="4" width="18" height="16" rx="2" />
    <circle cx="9" cy="10" r="2" />
    <path d="m21 16-5-5-9 9" />
  </I>
);
export const IconPaperclip = (p: P) => (
  <I {...p}>
    <path d="m21 11-8.6 8.6a5 5 0 0 1-7-7L14 4a3.3 3.3 0 0 1 4.7 4.7l-8.6 8.6a1.7 1.7 0 0 1-2.4-2.4L15.5 7" />
  </I>
);
export const IconAt = (p: P) => (
  <I {...p}>
    <circle cx="12" cy="12" r="4" />
    <path d="M16 8v5a3 3 0 0 0 6 0v-1a10 10 0 1 0-4 8" />
  </I>
);
export const IconZap = (p: P) => (
  <I {...p}>
    <path d="M13 2 4 14h7l-1 8 9-12h-7z" />
  </I>
);
export const IconUndo = (p: P) => (
  <I {...p}>
    <path d="M9 14 4 9l5-5" />
    <path d="M4 9h11a5 5 0 0 1 0 10h-4" />
  </I>
);
export const IconFork = (p: P) => (
  <I {...p}>
    <circle cx="6" cy="5" r="2" />
    <circle cx="18" cy="5" r="2" />
    <circle cx="12" cy="19" r="2" />
    <path d="M6 7v2a3 3 0 0 0 3 3h6a3 3 0 0 0 3-3V7M12 12v5" />
  </I>
);
export const IconCommit = (p: P) => (
  <I {...p}>
    <circle cx="12" cy="12" r="3.5" />
    <path d="M2 12h6.5M15.5 12H22" />
  </I>
);
export const IconUpload = (p: P) => (
  <I {...p}>
    <path d="M12 16V4M6 10l6-6 6 6M4 20h16" />
  </I>
);
export const IconPR = (p: P) => (
  <I {...p}>
    <circle cx="6" cy="6" r="2" />
    <circle cx="6" cy="18" r="2" />
    <circle cx="18" cy="18" r="2" />
    <path d="M6 8v8M18 16V9a3 3 0 0 0-3-3h-4M13 3l-2 3 2 3" />
  </I>
);
export const IconKeyboard = (p: P) => (
  <I {...p}>
    <rect x="2" y="6" width="20" height="12" rx="2" />
    <path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M7 14h10" />
  </I>
);
export const IconCommand = (p: P) => (
  <I {...p}>
    <path d="M9 6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3z" />
  </I>
);
export const IconMessage = (p: P) => (
  <I {...p}>
    <path d="M21 12a8 8 0 0 1-11.6 7.1L4 20l1-4.6A8 8 0 1 1 21 12z" />
  </I>
);
export const IconDiff = (p: P) => (
  <I {...p}>
    <path d="M12 3v8M8 7h8M8 17h8" />
    <rect x="3" y="2" width="18" height="20" rx="2" />
  </I>
);

export const IconSparkle = (p: P) => (
  <I {...p}>
    <path d="M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8z" />
    <path d="M19 15l.8 2.2L22 18l-2.2.8L19 21l-.8-2.2L16 18l2.2-.8z" />
  </I>
);
export const IconGauge = (p: P) => (
  <I {...p}>
    <path d="M4 18a8 8 0 1 1 16 0" />
    <path d="m12 14 4-5" />
  </I>
);
export const IconBot = (p: P) => (
  <I {...p}>
    <rect x="4" y="8" width="16" height="12" rx="3" />
    <path d="M12 4v4M9 13h.01M15 13h.01M9.5 17h5" />
  </I>
);

export function ToolKindIcon({ kind, size = 14 }: { kind?: string; size?: number }) {
  switch (kind) {
    case "read":
      return <IconFile size={size} />;
    case "edit":
      return <IconEdit size={size} />;
    case "delete":
      return <IconTrash size={size} />;
    case "move":
      return <IconMove size={size} />;
    case "search":
      return <IconSearch size={size} />;
    case "execute":
      return <IconTerminal size={size} />;
    case "think":
      return <IconBrain size={size} />;
    case "fetch":
      return <IconGlobe size={size} />;
    default:
      return <IconTool size={size} />;
  }
}

export const LogoMark = ({ size = 18 }: { size?: number }) => (
  <svg width={size} height={size} viewBox="0 0 32 32" aria-hidden="true">
    <path d="M16 5 27 25H5z" fill="none" stroke="var(--accent)" strokeWidth="2.8" strokeLinejoin="round" />
    <circle cx="16" cy="19" r="2.8" fill="var(--green)" />
  </svg>
);
