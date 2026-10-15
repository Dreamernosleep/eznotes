import React from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { X } from "lucide-react";
import { ShortcutInput } from "./ShortcutInput";

interface MeetingSettingProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

const MAX_LENGTH_MINUTES = [30, 60, 120, 180];
const AUDIO_RETENTION_DAYS = [1, 7, 30, 0];

export const MeetingMaxLength: React.FC<MeetingSettingProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const value = String(getSetting("meeting_max_minutes") ?? 120);

  return (
    <SettingContainer
      title={t("settings.meetings.maxLength.title")}
      description={t("settings.meetings.maxLength.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      <Dropdown
        options={MAX_LENGTH_MINUTES.map((minutes) => ({
          value: String(minutes),
          label:
            minutes < 60
              ? t("settings.meetings.maxLength.minutes", { count: minutes })
              : t("settings.meetings.maxLength.hours", {
                  count: minutes / 60,
                }),
        }))}
        selectedValue={value}
        onSelect={(v) => updateSetting("meeting_max_minutes", Number(v))}
        disabled={isUpdating("meeting_max_minutes")}
      />
    </SettingContainer>
  );
};

export const MeetingAudioRetention: React.FC<MeetingSettingProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const value = String(getSetting("meeting_audio_retention_days") ?? 7);

  return (
    <SettingContainer
      title={t("settings.meetings.audioRetention.title")}
      description={t("settings.meetings.audioRetention.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      <Dropdown
        options={AUDIO_RETENTION_DAYS.map((days) => ({
          value: String(days),
          label:
            days === 0
              ? t("settings.meetings.audioRetention.forever")
              : t("settings.meetings.audioRetention.days", { count: days }),
        }))}
        selectedValue={value}
        onSelect={(v) =>
          updateSetting("meeting_audio_retention_days", Number(v))
        }
        disabled={isUpdating("meeting_audio_retention_days")}
      />
    </SettingContainer>
  );
};

export const MeetingAutoDetect: React.FC<MeetingSettingProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  return (
    <ToggleSwitch
      checked={getSetting("meeting_auto_detect") ?? true}
      onChange={(enabled) => updateSetting("meeting_auto_detect", enabled)}
      isUpdating={isUpdating("meeting_auto_detect")}
      label={t("settings.meetings.autoDetect.label")}
      description={t("settings.meetings.autoDetect.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      tooltipPosition="bottom"
    />
  );
};

export const MeetingScreenNames: React.FC<MeetingSettingProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  return (
    <ToggleSwitch
      checked={getSetting("meeting_screen_names") ?? true}
      onChange={(enabled) => updateSetting("meeting_screen_names", enabled)}
      isUpdating={isUpdating("meeting_screen_names")}
      label={t("settings.meetings.screenNames.label")}
      description={t("settings.meetings.screenNames.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      tooltipPosition="bottom"
    />
  );
};

/** Apps the user chose "Never" for; each can be removed again. */
export const MeetingIgnoredApps: React.FC<MeetingSettingProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const apps = getSetting("meeting_ignored_apps") ?? [];
  if (apps.length === 0) return null;
  return (
    <SettingContainer
      title={t("settings.meetings.ignoredApps.title")}
      description={t("settings.meetings.ignoredApps.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      <div className="flex flex-wrap justify-end gap-2">
        {apps.map((app) => (
          <span
            key={app}
            className="flex items-center gap-1 px-2 py-0.5 rounded-full border border-mid-gray/30 text-xs"
          >
            {app}
            <button
              disabled={isUpdating("meeting_ignored_apps")}
              onClick={() =>
                updateSetting(
                  "meeting_ignored_apps",
                  apps.filter((a) => a !== app),
                )
              }
              title={t("settings.meetings.ignoredApps.remove")}
              className="text-text/40 hover:text-text cursor-pointer"
            >
              <X className="w-3 h-3" />
            </button>
          </span>
        ))}
      </div>
    </SettingContainer>
  );
};

export const MeetingSettings: React.FC<MeetingSettingProps> = (props) => (
  <>
    <MeetingAutoDetect {...props} />
    <MeetingIgnoredApps {...props} />
    <MeetingScreenNames {...props} />
    <ShortcutInput shortcutId="toggle_meeting" {...props} />
    <MeetingMaxLength {...props} />
    <MeetingAudioRetention {...props} />
  </>
);
