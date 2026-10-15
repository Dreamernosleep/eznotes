import React, { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { Circle, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { commands, type DetectedMeeting } from "@/bindings";

/** "Record this meeting?" card shown when a meeting app uses the mic. */
const MeetingPrompt: React.FC = () => {
  const { t } = useTranslation();
  const [meeting, setMeeting] = useState<DetectedMeeting | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    commands.meetingPromptCurrent().then(setMeeting);
    const unlisten = listen<DetectedMeeting | null>("meeting-prompt", (e) =>
      setMeeting(e.payload),
    );
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  if (!meeting) return null;

  const record = async () => {
    setBusy(true);
    try {
      await commands.meetingPromptRecord();
    } finally {
      setBusy(false);
    }
  };

  return (
    // The padding leaves room for the shadow inside the transparent window,
    // so its edge is never clipped into a hard line.
    <div className="h-full w-full p-3">
      <div className="h-full w-full rounded-2xl border border-mid-gray/15 bg-background shadow-[0_4px_14px_rgba(0,0,0,0.14)] px-4 py-3 flex flex-col justify-between text-text">
        <div className="flex items-start justify-between gap-2">
          <div>
            <p className="text-sm font-semibold">
              {t("meetingPrompt.title", { app: meeting.app })}
            </p>
            <p className="text-xs text-text/60">{t("meetingPrompt.hint")}</p>
          </div>
          <button
            onClick={() => commands.meetingPromptDismiss()}
            title={t("meetingPrompt.notNow")}
            className="text-text/40 hover:text-text cursor-pointer"
          >
            <X className="w-4 h-4" />
          </button>
        </div>
        <div className="flex items-center justify-between gap-2">
          <button
            onClick={() => commands.meetingPromptNever()}
            className="text-xs text-text/50 hover:text-text underline-offset-2 hover:underline cursor-pointer"
          >
            {t("meetingPrompt.never", { app: meeting.app })}
          </button>
          <div className="flex gap-2">
            <button
              onClick={() => commands.meetingPromptDismiss()}
              className="px-3 py-1 text-sm rounded-md text-text/70 hover:bg-mid-gray/15 cursor-pointer"
            >
              {t("meetingPrompt.notNow")}
            </button>
            <button
              onClick={record}
              disabled={busy}
              className="px-3 py-1 text-sm rounded-md bg-background-ui text-white font-medium flex items-center gap-1.5 hover:bg-background-ui/80 disabled:opacity-50 cursor-pointer"
            >
              <Circle className="w-2.5 h-2.5 fill-current" />
              {t("meetingPrompt.record")}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
};

export default MeetingPrompt;
