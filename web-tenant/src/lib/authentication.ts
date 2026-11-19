import { useEffect, useState } from "react";
import { ApiError, call, finishPasskeyLogin } from "./api";

export interface AuthenticationOptions {
  email_verification: boolean;
  turnstile_site_key: string | null;
  passkey: boolean;
  github: boolean;
  google: boolean;
}

const PASSWORD_ONLY: AuthenticationOptions = {
  email_verification: false, turnstile_site_key: null, passkey: false, github: false, google: false,
};

let pendingOptions: Promise<AuthenticationOptions> | undefined;

function readOptions(): Promise<AuthenticationOptions> {
  pendingOptions ??= call<AuthenticationOptions>("GET", "/auth/options").then(
    ({ data }) => data,
    (error: unknown) => {
      // 支持滚动更新时先提供新版页面、控制面仍提供原有口令登录的情况。
      if (error instanceof ApiError && error.status === 404) return PASSWORD_ONLY;
      throw error;
    },
  ).finally(() => { pendingOptions = undefined; });
  return pendingOptions;
}

export function useAuthenticationOptions(enabled = true) {
  const [options, setOptions] = useState<AuthenticationOptions | null>(null);
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    readOptions().then(
      (value) => { if (live) setOptions(value); },
      () => { if (live) setFailed(true); },
    );
    return () => { live = false; };
  }, [enabled, attempt]);
  return {
    options, failed,
    available: options !== null && options !== PASSWORD_ONLY,
    retry: () => {
      setFailed(false);
      setOptions(null);
      setAttempt((value) => value + 1);
    },
  };
}

export const SOCIAL_PROVIDERS = ["github", "google"] as const;
export type SocialProvider = typeof SOCIAL_PROVIDERS[number];
export const SOCIAL_NAMES: Record<SocialProvider, string> = { github: "GitHub", google: "Google" };

export async function sendEmailCode(email: string, turnstileToken?: string): Promise<void> {
  await call<null>("POST", "/auth/email-code", { email, turnstile_token: turnstileToken });
}

export async function openSocialAuthorization(
  provider: SocialProvider,
  options: { mode: "login"; turnstile_token?: string } | { mode: "link"; password: string },
): Promise<void> {
  const { data } = await call<{ url: string }>("POST", `/auth/oauth/${provider}/start`, options);
  window.location.assign(data.url);
}

export async function socialConnections(): Promise<Record<SocialProvider, boolean>> {
  return (await call<Record<SocialProvider, boolean>>("GET", "/auth/oauth")).data;
}

export async function disconnectSocial(provider: SocialProvider, password: string): Promise<void> {
  await call<null>("DELETE", `/auth/oauth/${provider}`, { password });
}

export interface Passkey {
  id: string;
  name: string;
  created_at: string;
  last_used_at: string | null;
}

interface Ceremony<T> {
  challenge_id: string;
  options: { publicKey: T };
}

// webauthn-rs 的扩展输入不含 largeBlob 和 PRF 两种需进一步解码的二进制字段。
type ExtensionInputs = Omit<AuthenticationExtensionsClientInputs, "largeBlob" | "prf">;
type DescriptorJson = Omit<PublicKeyCredentialDescriptor, "id"> & { id: string };
type CreationJson = Omit<PublicKeyCredentialCreationOptions, "challenge" | "user" | "excludeCredentials" | "extensions"> & {
  challenge: string;
  user: Omit<PublicKeyCredentialUserEntity, "id"> & { id: string };
  excludeCredentials?: DescriptorJson[];
  extensions?: ExtensionInputs;
};
type RequestJson = Omit<PublicKeyCredentialRequestOptions, "challenge" | "allowCredentials" | "extensions"> & {
  challenge: string;
  allowCredentials?: DescriptorJson[];
  extensions?: ExtensionInputs;
};

export function supportsPasskeys(): boolean {
  return window.isSecureContext && typeof PublicKeyCredential !== "undefined" && Boolean(navigator.credentials);
}

function decodeBytes(value: string): ArrayBuffer {
  return Uint8Array.from(atob(value.replaceAll("-", "+").replaceAll("_", "/")), (byte) => byte.charCodeAt(0)).buffer;
}

function encodeBytes(value: ArrayBuffer): string {
  return btoa(Array.from(new Uint8Array(value), (byte) => String.fromCharCode(byte)).join(""))
    .replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function descriptors(values?: DescriptorJson[]): PublicKeyCredentialDescriptor[] | undefined {
  return values?.map((value) => ({ ...value, id: decodeBytes(value.id) }));
}

function creationOptions(options: CreationJson): PublicKeyCredentialCreationOptions {
  if (PublicKeyCredential.parseCreationOptionsFromJSON) return PublicKeyCredential.parseCreationOptionsFromJSON(options);
  // Safari 等旧版浏览器支持 WebAuthn，但没有 JSON 转换方法。
  return {
    ...options,
    challenge: decodeBytes(options.challenge),
    user: { ...options.user, id: decodeBytes(options.user.id) },
    excludeCredentials: descriptors(options.excludeCredentials),
  };
}

function requestOptions(options: RequestJson): PublicKeyCredentialRequestOptions {
  if (PublicKeyCredential.parseRequestOptionsFromJSON) return PublicKeyCredential.parseRequestOptionsFromJSON(options);
  return { ...options, challenge: decodeBytes(options.challenge), allowCredentials: descriptors(options.allowCredentials) };
}

function credentialJson(result: Credential | null): unknown {
  if (!(result instanceof PublicKeyCredential)) throw new Error("未取得 Passkey 凭据，请重试。");
  if (result.toJSON) return result.toJSON();
  const response = result.response;
  const clientDataJSON = encodeBytes(response.clientDataJSON);
  const data = "attestationObject" in response
    ? attestationResponse(response as AuthenticatorAttestationResponse, clientDataJSON)
    : assertionResponse(response as AuthenticatorAssertionResponse, clientDataJSON);
  return {
    type: result.type, id: result.id, rawId: encodeBytes(result.rawId), response: data,
    clientExtensionResults: result.getClientExtensionResults(),
  };
}

function attestationResponse(response: AuthenticatorAttestationResponse, clientDataJSON: string) {
  return {
    clientDataJSON,
    attestationObject: encodeBytes(response.attestationObject),
    transports: response.getTransports?.(),
  };
}

function assertionResponse(response: AuthenticatorAssertionResponse, clientDataJSON: string) {
  return {
    clientDataJSON,
    authenticatorData: encodeBytes(response.authenticatorData),
    signature: encodeBytes(response.signature),
    userHandle: response.userHandle ? encodeBytes(response.userHandle) : null,
  };
}

export async function passkeys(): Promise<Passkey[]> {
  return (await call<{ passkeys: Passkey[] }>("GET", "/auth/passkeys")).data.passkeys;
}

export async function registerPasskey(name: string, password: string): Promise<void> {
  const { data } = await call<Ceremony<CreationJson>>("POST", "/auth/passkeys/register/start", { name, password });
  const credential = await navigator.credentials.create({ publicKey: creationOptions(data.options.publicKey) });
  await call("POST", "/auth/passkeys/register/finish", { challenge_id: data.challenge_id, credential: credentialJson(credential) });
}

export async function loginWithPasskey(email: string, turnstileToken?: string) {
  const { data } = await call<Ceremony<RequestJson>>("POST", "/auth/passkeys/login/start", { email, turnstile_token: turnstileToken });
  const credential = await navigator.credentials.get({ publicKey: requestOptions(data.options.publicKey) });
  return finishPasskeyLogin(data.challenge_id, credentialJson(credential));
}

export async function removePasskey(id: string, password: string): Promise<void> {
  await call<null>("DELETE", `/auth/passkeys/${encodeURIComponent(id)}`, { password });
}

export function browserAuthenticationError(error: unknown): string {
  if (error instanceof DOMException) {
    if (error.name === "NotAllowedError" || error.name === "AbortError") {
      return "未完成 Passkey 验证，请重试或使用口令。";
    }
    if (error.name === "InvalidStateError") return "此 Passkey 已添加。";
    return "当前设备无法完成 Passkey 验证，请确认浏览器与设备支持。";
  }
  return error instanceof Error ? error.message : "操作失败，请重试。";
}
