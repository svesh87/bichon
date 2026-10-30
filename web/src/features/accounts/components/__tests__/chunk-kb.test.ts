// svesh: the per-account chunk size of the chunked body download.
import { describe, it, expect } from 'vitest'
import { getAccountSchema } from '../schema'

const t = (key: string) => key

const baseData = {
  email: 'test@example.com',
  imap: {
    host: 'imap.example.com',
    port: 993,
    encryption: 'Ssl' as const,
    auth: {
      auth_type: 'Password' as const,
      password: 'mypassword',
    },
  },
  enabled: true,
  use_dangerous: false,
  download_interval_min: 60,
  download_batch_size: 30,
  max_email_size_bytes: 10 * 1024 * 1024,
  auto_download_new_mailboxes: true,
}

describe('Account Schema - imap_fetch_chunk_kb', () => {
  const schema = getAccountSchema(false, t)

  it('is optional (server default)', () => {
    expect(schema.safeParse(baseData).success).toBe(true)
  })

  it.each([0, 4, 128, 65536])('accepts %d', (value) => {
    expect(schema.safeParse({ ...baseData, imap_fetch_chunk_kb: value }).success).toBe(true)
  })

  it.each([-1, 65537, 1.5, NaN])('rejects %d', (value) => {
    const result = schema.safeParse({ ...baseData, imap_fetch_chunk_kb: value })
    expect(result.success).toBe(false)
    if (!result.success) {
      expect(result.error.issues[0].message).toBe('validation.imapFetchChunkKbInvalid')
    }
  })
})
