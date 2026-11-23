import { ApiError } from './api';
import { t, tFor } from './i18n';

/** The text of an error: the server's fixed word as a message of the catalogue. */
export function errorText(e: unknown): string {
  return e instanceof ApiError ? tFor('error.', e.code, 'error.unknown') : t('error.unknown');
}
