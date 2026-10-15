import React, { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  ArrowLeft,
  Check,
  Circle,
  Copy,
  FolderOpen,
  Pencil,
  RotateCcw,
  Square,
  Trash2,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  commands,
  type MeetingDetail,
  type MeetingSegment,
  type MeetingStatus,
  type MeetingSummary,
} from "@/bindings";
import { formatDateTime } from "@/utils/dateFormat";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { Textarea } from "../../ui/Textarea";
import { copyToClipboard } from "./clipboard";

/** Payload of the backend's `meeting-segment` event (a live line). */
type LiveSegment = MeetingSegment & { meeting_id: number };

const NOTES_SAVE_DELAY_MS = 800;

/** "You" uses the app colour; other speakers get these, in order. */
const YOU_COLOR = "text-logo-primary";
const SPEAKER_COLORS = [
  "text-sky-500",
  "text-amber-500",
  "text-emerald-500",
  "text-rose-500",
  "text-violet-500",
  "text-teal-500",
  "text-orange-500",
  "text-fuchsia-500",
];

const clock = (ms: number): string => {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const mm = String(m).padStart(2, "0");
  const ss = String(s).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
};

const formatStart = (startedAtMs: number, locale: string) =>
  formatDateTime(String(Math.floor(startedAtMs / 1000)), locale);

const IconButton: React.FC<{
  onClick: () => void;
  title: string;
  disabled?: boolean;
  children: React.ReactNode;
}> = ({ onClick, title, disabled, children }) => (
  <button
    onClick={onClick}
    disabled={disabled}
    title={title}
    className="p-1.5 rounded-md flex items-center justify-center transition-colors cursor-pointer text-text/50 hover:text-logo-primary disabled:cursor-not-allowed disabled:text-text/20"
  >
    {children}
  </button>
);

const StatusLabel: React.FC<{ status: string }> = ({ status }) => {
  const { t } = useTranslation();
  if (status === "complete") return null;
  const tone =
    status === "failed"
      ? "text-red-500"
      : status === "recording"
        ? "text-red-500"
        : "text-text/50";
  return (
    <span className={`text-xs ${tone}`}>
      {t(`settings.meetings.status.${status}`, { defaultValue: status })}
    </span>
  );
};

/** "● Recording 12:34 [Stop]" while a meeting records, else a Record button. */
const RecordingBar: React.FC<{ status: MeetingStatus | null }> = ({
  status,
}) => {
  const { t } = useTranslation();
  const [now, setNow] = useState(Date.now());
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!status?.recording) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [status?.recording]);

  const run = async (
    action: () => Promise<{ status: string; error?: unknown }>,
  ) => {
    setBusy(true);
    try {
      const result = await action();
      if (result.status === "error") toast.error(String(result.error));
    } finally {
      setBusy(false);
    }
  };

  if (status?.recording) {
    return (
      <div className="px-4 py-3 flex items-center justify-between bg-red-500/10 border border-red-500/30 rounded-lg">
        <div className="flex items-center gap-2 text-sm">
          <Circle className="w-3 h-3 fill-red-500 text-red-500 animate-pulse" />
          <span className="font-medium">
            {t("settings.meetings.recordingNow")}
          </span>
          <span className="tabular-nums text-text/70">
            {clock(now - (status.started_at ?? now))}
          </span>
          {!status.system_audio && (
            <span className="text-xs text-text/50">
              {t("settings.meetings.micOnly")}
            </span>
          )}
        </div>
        <Button
          variant="danger"
          size="sm"
          disabled={busy}
          onClick={() => run(() => commands.stopMeetingRecording())}
          className="flex items-center gap-2"
        >
          <Square className="w-3.5 h-3.5" />
          {t("settings.meetings.stop")}
        </Button>
      </div>
    );
  }

  return (
    <div className="px-4 py-3 flex items-center justify-between bg-background border border-mid-gray/20 rounded-lg">
      <span className="text-sm text-text/60">
        {t("settings.meetings.recordHint")}
      </span>
      <Button
        variant="primary-soft"
        size="sm"
        disabled={busy}
        onClick={() => run(() => commands.startMeetingRecording(null))}
        className="flex items-center gap-2"
      >
        <Circle className="w-3 h-3 fill-current" />
        {t("settings.meetings.record")}
      </Button>
    </div>
  );
};

export const MeetingsHistory: React.FC = () => {
  const { t, i18n } = useTranslation();
  const [meetings, setMeetings] = useState<MeetingSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [status, setStatus] = useState<MeetingStatus | null>(null);
  const [selectedId, setSelectedId] = useState<number | null>(null);

  const refresh = useCallback(async () => {
    const result = await commands.listMeetings();
    if (result.status === "ok") setMeetings(result.data);
    setLoading(false);
  }, []);

  useEffect(() => {
    refresh();
    commands.getMeetingStatus().then(setStatus);
    const unlisteners = [
      listen<MeetingStatus>("meeting-status", (e) => {
        setStatus(e.payload);
        refresh();
      }),
      listen<number>("meeting-processed", () => refresh()),
      listen<string>("meeting-error", (e) => toast.error(e.payload)),
      listen<number>("meeting-length-warning", (e) =>
        toast.warning(
          t("settings.meetings.lengthWarning", {
            minutes: Math.round(e.payload / 60),
          }),
        ),
      ),
    ];
    return () => {
      unlisteners.forEach((p) => p.then((fn) => fn()));
    };
  }, [refresh, t]);

  if (selectedId !== null) {
    return (
      <MeetingPage
        id={selectedId}
        recording={
          status?.recording === true && status.meeting_id === selectedId
        }
        onBack={() => {
          setSelectedId(null);
          refresh();
        }}
      />
    );
  }

  return (
    <div className="space-y-3">
      <RecordingBar status={status} />
      <div className="bg-background border border-mid-gray/20 rounded-lg overflow-visible">
        {loading ? (
          <div className="px-4 py-3 text-center text-text/60">
            {t("settings.history.loading")}
          </div>
        ) : meetings.length === 0 ? (
          <div className="px-4 py-3 text-center text-text/60">
            {t("settings.meetings.empty")}
          </div>
        ) : (
          <div className="divide-y divide-mid-gray/20">
            {meetings.map((m) => (
              <button
                key={m.id}
                onClick={() => setSelectedId(m.id)}
                className="w-full text-left px-4 py-3 flex flex-col gap-1 hover:bg-mid-gray/10 transition-colors cursor-pointer"
              >
                <div className="flex items-center justify-between gap-3">
                  <span className="text-sm font-medium truncate">
                    {m.title}
                  </span>
                  <div className="flex items-center gap-3 shrink-0">
                    <StatusLabel status={m.status} />
                    <span className="text-xs tabular-nums text-text/50">
                      {clock(m.duration_ms)}
                    </span>
                  </div>
                </div>
                <div className="flex items-center justify-between gap-3 text-xs text-text/50">
                  <span>{formatStart(m.started_at, i18n.language)}</span>
                  <span className="truncate">{m.speakers.join(" · ")}</span>
                </div>
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
};

const MeetingPage: React.FC<{
  id: number;
  recording: boolean;
  onBack: () => void;
}> = ({ id, recording, onBack }) => {
  const { t, i18n } = useTranslation();
  const [meeting, setMeeting] = useState<MeetingDetail | null>(null);
  const [notes, setNotes] = useState("");
  const [editingTitle, setEditingTitle] = useState(false);
  const [title, setTitle] = useState("");
  const [renaming, setRenaming] = useState<string | null>(null);
  const [speakerName, setSpeakerName] = useState("");
  const [copied, setCopied] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const saveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const load = useCallback(async () => {
    const result = await commands.getMeeting(id);
    if (result.status === "ok") {
      setMeeting(result.data);
      return result.data;
    }
    toast.error(String(result.error));
    return null;
  }, [id]);

  useEffect(() => {
    load().then((m) => {
      if (m) {
        setNotes(m.notes);
        setTitle(m.summary.title);
      }
    });
    const unlisteners = [
      listen<LiveSegment>("meeting-segment", (e) => {
        if (e.payload.meeting_id !== id) return;
        setMeeting((prev) =>
          prev && prev.transcript_kind === "live"
            ? {
                ...prev,
                segments: [...prev.segments, e.payload].sort(
                  (a, b) => a.t0_ms - b.t0_ms,
                ),
              }
            : prev,
        );
      }),
      listen<number>("meeting-processed", (e) => {
        if (e.payload === id) load();
      }),
      listen<MeetingStatus>("meeting-status", () => load()),
    ];
    return () => {
      unlisteners.forEach((p) => p.then((fn) => fn()));
      if (saveTimer.current) clearTimeout(saveTimer.current);
    };
  }, [id, load]);

  const onNotesChange = (value: string) => {
    setNotes(value);
    if (saveTimer.current) clearTimeout(saveTimer.current);
    saveTimer.current = setTimeout(async () => {
      const result = await commands.updateMeetingNotes(id, value);
      if (result.status === "error") toast.error(String(result.error));
    }, NOTES_SAVE_DELAY_MS);
  };

  const saveTitle = async () => {
    setEditingTitle(false);
    if (!meeting || title.trim() === meeting.summary.title) return;
    const result = await commands.renameMeeting(id, title);
    if (result.status === "error") toast.error(String(result.error));
    load();
  };

  const saveSpeaker = async (name: string = speakerName) => {
    const key = renaming;
    setRenaming(null);
    const current = meeting?.speakers.find((s) => s.key === key)?.name;
    if (!key || !name.trim() || name.trim() === current) return;
    const result = await commands.renameMeetingSpeaker(id, key, name);
    if (result.status === "error") toast.error(String(result.error));
    load();
  };

  const copyMarkdown = async () => {
    const result = await commands.getMeetingMarkdown(id);
    if (result.status !== "ok" || !(await copyToClipboard(result.data))) {
      toast.error(t("settings.meetings.copyError"));
      return;
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  const retry = async () => {
    const result = await commands.retryMeetingProcessing(id);
    if (result.status === "error") toast.error(String(result.error));
    load();
  };

  // First click arms, a second click within 4 s deletes.
  const remove = async () => {
    if (!confirmDelete) {
      setConfirmDelete(true);
      setTimeout(() => setConfirmDelete(false), 4000);
      return;
    }
    const result = await commands.deleteMeeting(id);
    if (result.status === "error") {
      toast.error(String(result.error));
      return;
    }
    onBack();
  };

  if (!meeting) {
    return (
      <div className="px-4 py-3 text-center text-text/60">
        {t("settings.history.loading")}
      </div>
    );
  }

  const { summary } = meeting;
  const names = new Map(meeting.speakers.map((s) => [s.key, s.name]));
  const others = meeting.speakers.filter((sp) => sp.key !== "you");
  const colorOf = (key: string) => {
    if (key === "you") return YOU_COLOR;
    const index = others.findIndex((sp) => sp.key === key);
    return SPEAKER_COLORS[Math.max(index, 0) % SPEAKER_COLORS.length];
  };
  const nameOf = (key: string) =>
    names.get(key) ??
    (key === "you"
      ? t("settings.meetings.you")
      : t("settings.meetings.others"));
  const busy = recording || summary.status === "processing";

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-2">
        <Button
          variant="ghost"
          size="sm"
          onClick={onBack}
          className="flex items-center gap-1"
        >
          <ArrowLeft className="w-4 h-4" />
          {t("settings.meetings.back")}
        </Button>
        <div className="flex items-center">
          <IconButton
            onClick={copyMarkdown}
            title={t("settings.meetings.copyMarkdown")}
          >
            {copied ? (
              <Check width={16} height={16} />
            ) : (
              <Copy width={16} height={16} />
            )}
          </IconButton>
          <IconButton
            onClick={() => commands.openMeetingFolder(id)}
            title={t("settings.meetings.openFolder")}
          >
            <FolderOpen width={16} height={16} />
          </IconButton>
          <IconButton
            onClick={retry}
            disabled={busy}
            title={t("settings.meetings.retry")}
          >
            <RotateCcw width={16} height={16} />
          </IconButton>
          <IconButton
            onClick={remove}
            disabled={recording}
            title={
              confirmDelete
                ? t("settings.meetings.deleteConfirm")
                : t("settings.meetings.delete")
            }
          >
            <Trash2
              width={16}
              height={16}
              className={confirmDelete ? "text-red-500" : undefined}
            />
          </IconButton>
        </div>
      </div>

      <div className="px-1 space-y-1">
        {editingTitle ? (
          <Input
            autoFocus
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            onBlur={saveTitle}
            onKeyDown={(e) => e.key === "Enter" && saveTitle()}
            className="w-full"
          />
        ) : (
          <button
            onClick={() => setEditingTitle(true)}
            className="group flex items-center gap-2 text-lg font-semibold cursor-pointer"
            title={t("settings.meetings.renameTitle")}
          >
            {summary.title}
            <Pencil className="w-3.5 h-3.5 text-text/30 group-hover:text-logo-primary" />
          </button>
        )}
        <div className="flex items-center gap-3 text-xs text-text/50">
          <span>{formatStart(summary.started_at, i18n.language)}</span>
          <span className="tabular-nums">{clock(summary.duration_ms)}</span>
          {summary.source_app && <span>{summary.source_app}</span>}
          <StatusLabel status={recording ? "recording" : summary.status} />
        </div>
        {summary.status === "failed" && summary.error && (
          <p className="text-xs text-red-500">{summary.error}</p>
        )}
      </div>

      <section className="space-y-2">
        <h3 className="px-1 text-xs font-medium text-mid-gray uppercase tracking-wide">
          {t("settings.meetings.notes")}
        </h3>
        <Textarea
          value={notes}
          onChange={(e) => onNotesChange(e.target.value)}
          placeholder={t("settings.meetings.notesPlaceholder")}
          className="w-full"
        />
      </section>

      <section className="space-y-2">
        <div className="px-1 flex items-center justify-between gap-2">
          <h3 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
            {t("settings.meetings.transcript")}
          </h3>
          {meeting.transcript_kind === "live" && (
            <span className="text-xs text-text/50">
              {t("settings.meetings.liveNote")}
            </span>
          )}
        </div>

        {meeting.speakers.length > 0 && (
          <div className="px-1 flex flex-wrap gap-2">
            {meeting.speakers.map((s) =>
              renaming === s.key ? (
                <div
                  key={s.key}
                  className="flex flex-wrap items-center gap-1.5"
                >
                  <Input
                    autoFocus
                    value={speakerName}
                    onChange={(e) => setSpeakerName(e.target.value)}
                    onBlur={() => saveSpeaker()}
                    onKeyDown={(e) => e.key === "Enter" && saveSpeaker()}
                    className="w-36"
                  />
                  {meeting.participants.length > 0 && (
                    <span className="text-xs text-text/50">
                      {t("settings.meetings.participants")}:
                    </span>
                  )}
                  {meeting.participants.map((name) => (
                    <button
                      key={name}
                      // Keep the input focused so its blur doesn't save first.
                      onMouseDown={(e) => e.preventDefault()}
                      onClick={() => saveSpeaker(name)}
                      className="px-2 py-0.5 rounded-full bg-mid-gray/15 text-xs hover:bg-logo-primary/20 cursor-pointer"
                    >
                      {name}
                    </button>
                  ))}
                </div>
              ) : (
                <button
                  key={s.key}
                  onClick={() => {
                    setRenaming(s.key);
                    setSpeakerName(s.name);
                  }}
                  title={t("settings.meetings.renameSpeaker")}
                  className={`px-2 py-0.5 rounded-full border border-mid-gray/30 text-xs font-medium cursor-pointer hover:border-logo-primary ${colorOf(s.key)}`}
                >
                  {s.name}
                </button>
              ),
            )}
          </div>
        )}

        <div className="bg-background border border-mid-gray/20 rounded-lg px-4 py-3 space-y-3">
          {meeting.segments.length === 0 ? (
            <p className="text-sm text-text/50 text-center">
              {busy
                ? t("settings.meetings.waiting")
                : t("settings.meetings.noTranscript")}
            </p>
          ) : (
            meeting.segments.map((seg, i) => (
              <div key={`${seg.t0_ms}-${i}`} className="text-sm">
                <div className="flex items-baseline gap-2">
                  <span className={`font-medium ${colorOf(seg.speaker_key)}`}>
                    {nameOf(seg.speaker_key)}
                  </span>
                  <span className="text-xs tabular-nums text-text/40">
                    {clock(seg.t0_ms)}
                  </span>
                </div>
                <p className="text-text/90 select-text whitespace-pre-wrap break-words">
                  {seg.text}
                </p>
              </div>
            ))
          )}
        </div>
      </section>
    </div>
  );
};
