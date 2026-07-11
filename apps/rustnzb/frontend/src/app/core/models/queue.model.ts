export interface ServerArticleStats {
  server_id: string;
  server_name: string;
  articles_downloaded: number;
  articles_failed: number;
  bytes_downloaded: number;
}

export interface NzbJob {
  id: string;
  name: string;
  category: string;
  status: string;
  priority: number;
  total_bytes: number;
  downloaded_bytes: number;
  file_count: number;
  files_completed: number;
  article_count: number;
  articles_downloaded: number;
  articles_failed: number;
  added_at: string;
  completed_at: string | null;
  speed_bps: number;
  error_message: string | null;
  server_stats: ServerArticleStats[];
}

export interface QueueResponse {
  jobs: NzbJob[];
  total: number;
  speed_bps: number;
  paused: boolean;
}

export interface StatusResponse {
  version: string;
  speed_bps: number;
  speed_limit_bps: number;
  queue_size: number;
  disk_space_free: number;
  /** Total filesystem capacity in bytes; 0 means unknown (platform doesn't report it). */
  disk_space_total: number;
  min_free_space_bytes: number;
  paused: boolean;
  pause_remaining_secs: number | null;
  webdav_available: boolean;
  webdav_enabled: boolean;
}

export interface HistoryEntry {
  id: string;
  name: string;
  category: string;
  status: string;
  total_bytes: number;
  downloaded_bytes: number;
  added_at: string;
  completed_at: string;
  output_dir: string;
  stages: StageResult[];
  error_message: string | null;
  server_stats: ServerArticleStats[];
  has_nzb_data: boolean;
  duration_secs?: number;
  average_speed_bps?: number;
  articles_served?: number;
  articles_missing?: number;
}

export interface LogEntry {
  seq: number;
  timestamp: string;
  level: string;
  message: string;
}

export interface LogsResponse {
  entries: LogEntry[];
  latest_seq: number;
}

export interface StageResult {
  name: string;
  status: string;
  message: string | null;
  duration_secs: number;
}
