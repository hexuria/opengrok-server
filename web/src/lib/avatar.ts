// Avatar handling — read a picked file to a data: URL and guard it the same way the server does.
//
// The server (account_api::update_profile) accepts an avatar only when it is a `data:image/…` URL
// and at most 512 KB measured on the string length. We mirror both bounds here so a person gets an
// immediate, friendly refusal instead of a round-trip that ends in a 413/422.

/** The server's cap: 512 KB on the data-URL string. */
export const AVATAR_MAX_BYTES = 512 * 1024;

export type AvatarCheck = { ok: true } | { ok: false; reason: string };

/** Validate a data URL against the server's two rules. */
export function checkAvatar(dataUrl: string): AvatarCheck {
  if (!dataUrl.startsWith("data:image/")) {
    return { ok: false, reason: "That is not an image." };
  }
  if (dataUrl.length > AVATAR_MAX_BYTES) {
    return { ok: false, reason: "That image is too large (512 KB max)." };
  }
  return { ok: true };
}

/** Read a File into a data: URL. Rejects if the reader fails or yields a non-string result. */
export function fileToDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error("Could not read that file."));
    reader.onload = () => {
      const result = reader.result;
      if (typeof result === "string") resolve(result);
      else reject(new Error("Could not read that file."));
    };
    reader.readAsDataURL(file);
  });
}
