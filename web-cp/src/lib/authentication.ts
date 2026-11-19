import { call } from "./api";

export type OAuthProvider = "github" | "google";

type Email = { enabled: boolean; endpoint: string; from: string };
type Turnstile = { enabled: boolean; site_key: string };
type OAuth = { enabled: boolean; client_id: string };

type Common = { version: number; portal_origin: string; passkey: { enabled: boolean }; password_recovery_enabled?: boolean };
export type AuthenticationSettings = Common & {
  email: Email & { api_key_configured: boolean };
  turnstile: Turnstile & { secret_key_configured: boolean };
} & Record<OAuthProvider, OAuth & { client_secret_configured: boolean; callback_url: string }>;

export type AuthenticationDraft = Common & {
  email: Email & { api_key?: string };
  turnstile: Turnstile & { secret_key?: string };
} & Record<OAuthProvider, OAuth & { client_secret?: string }>;

const PATH = "/admin/settings/authentication";

export async function readAuthenticationSettings(signal: AbortSignal): Promise<AuthenticationSettings> {
  return (await call<AuthenticationSettings>("GET", PATH, undefined, undefined, signal)).data;
}

export async function writeAuthenticationSettings(draft: AuthenticationDraft): Promise<AuthenticationSettings> {
  return (await call<AuthenticationSettings>("PUT", PATH, draft)).data;
}

export function authenticationDraft(settings: AuthenticationSettings): AuthenticationDraft {
  const { version, portal_origin, email, password_recovery_enabled, turnstile, passkey, github, google } = settings;
  return {
    version,
    portal_origin,
    password_recovery_enabled,
    email: { enabled: email.enabled, endpoint: email.endpoint, from: email.from },
    turnstile: { enabled: turnstile.enabled, site_key: turnstile.site_key },
    passkey: { enabled: passkey.enabled },
    github: { enabled: github.enabled, client_id: github.client_id },
    google: { enabled: google.enabled, client_id: google.client_id },
  };
}
