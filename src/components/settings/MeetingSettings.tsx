import React from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
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

export const MeetingSettings: React.FC<MeetingSettingProps> = (props) => (
  <>
    <ShortcutInput shortcutId="toggle_meeting" {...props} />
    <MeetingMaxLength {...props} />
    <MeetingAudioRetention {...props} />
  </>
);
