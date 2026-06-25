import { type ReactNode, useState } from "react";
import type { IndicatorState } from "./status-state";
import {
  ActivityPulse,
  Avatar,
  Badge,
  Button,
  Checkbox,
  Dialog,
  DotmLoader,
  IconButton,
  Input,
  Select,
  SessionRow,
  SessionTab,
  StatusIndicator,
  Switch,
  Tag,
  Toast,
  Tooltip,
  useTheme,
} from "./ui";
import {
  ArrowUp,
  Bell,
  Check,
  GitBranch,
  More,
  Plus,
  Search,
  Settings,
  Terminal,
  Trash,
} from "./ui/icons";

/** No-op handler for the static demos (avoids empty-arrow lint). */
const noop = () => undefined;

/* ------------------------------------------------------------------ layout */

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="sc-sec">
      <h2 className="sc-sec__h">{title}</h2>
      {children}
    </section>
  );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="sc-row">
      <span className="sc-row__label">{label}</span>
      <div className="sc-row__items">{children}</div>
    </div>
  );
}

function Swatch({ token, on }: { token: string; on?: string }) {
  return (
    <div className="sc-swatch">
      <div
        className="sc-swatch__box"
        style={
          {
            background: `var(${token})`,
            color: on ? `var(${on})` : undefined,
          } as React.CSSProperties
        }
      />
      <span className="sc-swatch__name">{token}</span>
    </div>
  );
}

/* --------------------------------------------------------------- foundations */

const SURFACES = [
  "--bg-app",
  "--bg-sidebar",
  "--bg-panel",
  "--bg-elevated",
  "--bg-hover",
  "--bg-active",
  "--bg-input",
];
const ACCENTS = [
  "--accent",
  "--accent-hover",
  "--accent-press",
  "--accent-bright",
  "--accent-subtle",
];
const SEMANTIC = ["--success", "--warning", "--danger", "--info"];
const TEXT = [
  "--text-primary",
  "--text-secondary",
  "--text-muted",
  "--text-disabled",
];
const ANSI = [
  "--ansi-red",
  "--ansi-green",
  "--ansi-yellow",
  "--ansi-blue",
  "--ansi-magenta",
  "--ansi-cyan",
  "--ansi-white",
];
const TYPE_ROLES: { token: string; label: string }[] = [
  { token: "--type-display", label: "Display — start a session" },
  { token: "--type-heading", label: "Heading — files changed" },
  { token: "--type-title", label: "Title — refactor auth middleware" },
  {
    token: "--type-body",
    label: "Body — runs in a fresh remote sandbox attached to a workspace.",
  },
  { token: "--type-ui", label: "UI — sidebar / control text (13px)" },
  { token: "--type-meta", label: "Meta — 4m ago · needs you" },
];
const SPACES = [
  "--space-1",
  "--space-2",
  "--space-3",
  "--space-4",
  "--space-6",
  "--space-8",
  "--space-12",
  "--space-16",
];
const RADII = [
  "--radius-xs",
  "--radius-sm",
  "--radius-md",
  "--radius-lg",
  "--radius-xl",
];
const SHADOWS = [
  "--shadow-xs",
  "--shadow-sm",
  "--shadow-md",
  "--shadow-lg",
  "--shadow-popover",
];
const INDICATOR_STATES: IndicatorState[] = [
  "idle",
  "working",
  "needs",
  "done",
  "error",
];

/* ------------------------------------------------------------- interactive */

function SwitchDemo() {
  const [on, setOn] = useState(true);
  return (
    <Switch checked={on} onChange={setOn} label="Resolve via shell command" />
  );
}

function CheckboxDemo() {
  const [on, setOn] = useState(true);
  return (
    <Checkbox
      checked={on}
      onChange={setOn}
      label="Run as plain shell"
      description="Skip the agent launch command"
    />
  );
}

function DialogDemo() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button variant="secondary" onClick={() => setOpen(true)}>
        Open dialog
      </Button>
      {open && (
        <Dialog
          title="Start a new session"
          description="Runs in a fresh remote sandbox attached to a workspace."
          icon={<Terminal size={18} />}
          onClose={() => setOpen(false)}
          footer={
            <>
              <Button variant="ghost" onClick={() => setOpen(false)}>
                Cancel
              </Button>
              <Button variant="primary" onClick={() => setOpen(false)}>
                Create session
              </Button>
            </>
          }
        >
          <div style={{ display: "flex", flexDirection: "column", gap: 14 }}>
            <Input label="Session id" defaultValue="add-rate-limiting" mono />
            <Select label="Agent" options={["Claude Code", "Codex"]} />
          </div>
        </Dialog>
      )}
    </>
  );
}

/* -------------------------------------------------------------------- page */

export function Showcase() {
  const { theme, cycle } = useTheme();
  return (
    <div className="sc">
      <header className="sc__top">
        <span className="sc__title">Remora design system</span>
        <span className="sc__sub">live components · theme: {theme}</span>
        <span className="sc__spacer" />
        <Button variant="secondary" size="sm" onClick={cycle}>
          Cycle theme
        </Button>
      </header>

      <main className="sc__main">
        {/* ---------------------------------------------------- Foundations */}
        <Section title="Foundations">
          <Row label="Surfaces (dark-first; light follows the OS)">
            {SURFACES.map((t) => (
              <Swatch key={t} token={t} />
            ))}
          </Row>
          <Row label="Accent — only active session/tab, focus, primary, pulse">
            {ACCENTS.map((t) => (
              <Swatch key={t} token={t} />
            ))}
          </Row>
          <Row label="Semantic (chrome only — terminal owns its ANSI)">
            {SEMANTIC.map((t) => (
              <Swatch key={t} token={t} />
            ))}
          </Row>
          <Row label="Text">
            {TEXT.map((t) => (
              <div key={t} style={{ color: `var(${t})`, fontSize: 14 }}>
                <span style={{ fontFamily: "var(--font-mono)", fontSize: 11 }}>
                  {t}
                </span>
                <div>The quick brown fox</div>
              </div>
            ))}
          </Row>
          <Row label="Terminal ANSI (stays dark in every theme)">
            <div className="sc-termish">
              {ANSI.map((t) => (
                <div key={t} style={{ color: `var(${t})` }}>
                  {t} ➜ the agent says hello
                </div>
              ))}
            </div>
          </Row>
          <Row label="Type scale (Inter)">
            <div className="sc-typecol">
              {TYPE_ROLES.map((r) => (
                <div
                  key={r.token}
                  style={{ font: `var(${r.token})` } as React.CSSProperties}
                >
                  {r.label}
                </div>
              ))}
              <div style={{ font: "var(--type-code)" } as React.CSSProperties}>
                Mono — feat/auth-pool · claude-sonnet-4.6 · +142 / −38
              </div>
            </div>
          </Row>
          <Row label="Spacing (4px grid)">
            <div
              style={{
                display: "flex",
                flexDirection: "column",
                gap: 6,
                width: 320,
              }}
            >
              {SPACES.map((t) => (
                <div
                  key={t}
                  style={{ display: "flex", alignItems: "center", gap: 8 }}
                >
                  <span className="sc-swatch__name" style={{ width: 80 }}>
                    {t}
                  </span>
                  <div className="sc-spacebar" style={{ width: `var(${t})` }} />
                </div>
              ))}
            </div>
          </Row>
          <Row label="Radius">
            {RADII.map((t) => (
              <div
                key={t}
                style={{ display: "flex", flexDirection: "column", gap: 6 }}
              >
                <div
                  className="sc-radiusbox"
                  style={{ borderRadius: `var(${t})` }}
                />
                <span className="sc-swatch__name">{t}</span>
              </div>
            ))}
          </Row>
          <Row label="Elevation">
            {SHADOWS.map((t) => (
              <div
                key={t}
                className="sc-elevbox"
                style={{ boxShadow: `var(${t})` }}
              >
                {t.replace("--shadow-", "")}
              </div>
            ))}
          </Row>
        </Section>

        {/* ----------------------------------------------------------- Core */}
        <Section title="Core">
          <Row label="Button — variants">
            <Button variant="primary">Primary</Button>
            <Button variant="secondary">Secondary</Button>
            <Button variant="ghost">Ghost</Button>
            <Button variant="danger">Danger</Button>
          </Row>
          <Row label="Button — sizes, icon, loading, disabled, full">
            <Button size="sm">Small</Button>
            <Button size="md">Medium</Button>
            <Button size="lg">Large</Button>
            <Button icon={<Plus size={14} />}>With icon</Button>
            <Button loading>Loading</Button>
            <Button disabled>Disabled</Button>
            <div style={{ width: 200 }}>
              <Button fullWidth>Full width</Button>
            </div>
          </Row>
          <Row label="IconButton — sizes + active">
            <IconButton label="New" size="sm">
              <Plus size={14} />
            </IconButton>
            <IconButton label="Search">
              <Search size={16} />
            </IconButton>
            <IconButton label="Notifications" size="lg">
              <Bell size={18} />
            </IconButton>
            <IconButton label="Settings" active>
              <Settings size={16} />
            </IconButton>
          </Row>
          <Row label="Tag">
            <Tag>Claude Code</Tag>
            <Tag variant="branch" icon={<GitBranch size={12} />}>
              feat/auth-pool
            </Tag>
            <Tag onRemove={noop}>removable</Tag>
          </Row>
          <Row label="Badge — tones, dot, solid, count">
            <Badge>neutral</Badge>
            <Badge tone="success" dot>
              live
            </Badge>
            <Badge tone="warning">warning</Badge>
            <Badge tone="danger" dot>
              failed
            </Badge>
            <Badge tone="info">info</Badge>
            <Badge tone="accent" solid>
              new
            </Badge>
            <Badge count>12</Badge>
          </Row>
          <Row label="Avatar — host, rounded, circle, sizes, initials">
            <Avatar host name="studio · M2 Max" size="sm" />
            <Avatar name="acme/web-platform" />
            <Avatar name="Mara Vance" shape="circle" />
            <Avatar name="Codex" size="lg" />
          </Row>
        </Section>

        {/* ---------------------------------------------------------- Forms */}
        <Section title="Forms">
          <div className="sc-card" style={{ maxWidth: 420 }}>
            <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
              <Input label="Session id" defaultValue="add-rate-limiting" mono />
              <Input
                label="Base branch"
                placeholder="main"
                hint="Defaults to the project's configured base."
              />
              <Input
                label="Pod"
                defaultValue=""
                error="A pod name or command is required."
                mono
              />
              <Select label="Agent" options={["Claude Code", "Codex"]} />
              <Select
                label="Workspace"
                options={[
                  { value: "worktree", label: "Worktree (isolated)" },
                  { value: "shared", label: "Shared (in-place)" },
                ]}
              />
              <SwitchDemo />
              <Switch label="Disabled toggle" disabled />
              <CheckboxDemo />
              <Checkbox label="Indeterminate" indeterminate />
              <Checkbox label="Disabled" disabled />
            </div>
          </div>
        </Section>

        {/* ------------------------------------------------------- Feedback */}
        <Section title="Feedback">
          <Row label="Dialog">
            <DialogDemo />
          </Row>
          <Row label="Toast — tones">
            <Toast
              tone="success"
              icon={<Check size={16} />}
              title="Session created"
              message="Spinning up sandbox · acme/web-platform"
              actionLabel="Open"
              onAction={noop}
              onClose={noop}
            />
            <Toast
              tone="accent"
              icon={<Bell size={16} />}
              title="Agent needs input"
              message="Refactor auth middleware is waiting"
              actionLabel="Open session"
              onAction={noop}
              onClose={noop}
            />
            <Toast
              tone="danger"
              title="Spawn failed"
              message="transport: connection refused"
              onClose={noop}
            />
          </Row>
          <Row label="Tooltip (hover or focus the trigger)">
            <Tooltip content="Files & diff" kbd="⌘\" side="bottom">
              <Button variant="ghost" size="sm">
                Hover me
              </Button>
            </Tooltip>
          </Row>
        </Section>

        {/* -------------------------------------------------------- Session */}
        <Section title="Session — the brand's hero surfaces">
          <Row label="StatusIndicator — the canonical inline state dot">
            {INDICATOR_STATES.map((s) => (
              <div
                key={s}
                style={{ display: "flex", alignItems: "center", gap: 6 }}
              >
                <StatusIndicator state={s} />
                <span className="sc-swatch__name">{s}</span>
              </div>
            ))}
          </Row>
          <Row label="DotmLoader — the hero loader (working / needs animate)">
            {INDICATOR_STATES.map((s) => (
              <div
                key={s}
                style={{
                  display: "flex",
                  flexDirection: "column",
                  alignItems: "center",
                  gap: 8,
                }}
              >
                <DotmLoader state={s} size={56} />
                <span className="sc-swatch__name">{s}</span>
              </div>
            ))}
          </Row>
          <Row label="ActivityPulse — sizes sm / md / lg">
            {(["sm", "md", "lg"] as const).map((size) => (
              <div
                key={size}
                style={{ display: "flex", alignItems: "center", gap: 10 }}
              >
                {(["idle", "working", "needs", "done"] as IndicatorState[]).map(
                  (s) => (
                    <ActivityPulse key={s} state={s} size={size} />
                  ),
                )}
                <span className="sc-swatch__name">{size}</span>
              </div>
            ))}
          </Row>
          <Row label="SessionRow — as it renders in the sidebar (256px)">
            <div className="sc-sidebarish">
              <SessionRow
                name="refactor-auth"
                agent="Claude Code"
                branch="feat/auth-pool"
                state="working"
              />
              <SessionRow
                name="fix-e2e-retry"
                agent="Codex"
                state="needs"
                count={2}
              />
              <SessionRow
                name="bump-deps"
                agent="Claude Code"
                state="done"
                active
              />
              <SessionRow
                name="tune-cache"
                agent="Codex"
                state="idle"
                actions={
                  <IconButton label="Session actions" size="sm">
                    <More size={15} />
                  </IconButton>
                }
              />
            </div>
          </Row>
          <Row label="SessionTab — the workspace tab bar">
            <div className="sc-tabbarish">
              <SessionTab
                label="refactor-auth"
                state="working"
                active
                onClose={noop}
              />
              <SessionTab label="fix-e2e-retry" state="needs" onClose={noop} />
              <SessionTab label="bump-deps" state="done" dirty onClose={noop} />
              <SessionTab label="draft-migration" state="idle" onClose={noop} />
            </div>
          </Row>
          <Row label="Composer affordance reference (icons)">
            <IconButton label="Send">
              <ArrowUp size={16} />
            </IconButton>
            <IconButton label="Remove" size="sm">
              <Trash size={14} />
            </IconButton>
          </Row>
        </Section>
      </main>
    </div>
  );
}
