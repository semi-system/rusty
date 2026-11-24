import { describe, expect, it } from 'vitest';
import { cardBatches, eventClock, eventDay } from './format';

const card = (n: number) => `BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Person ${n}\r\nEND:VCARD`;

describe('cardBatches', () => {
  it('groups whole cards under the limit and ignores what is outside them', () => {
    const text = `junk\r\n${[1, 2, 3, 4].map(card).join('\r\n')}\r\ntrailing`;
    const one = cardBatches(text, 100000);
    expect(one).toHaveLength(1);
    expect(one[0]?.match(/BEGIN:VCARD/g)).toHaveLength(4);
    const many = cardBatches(text, card(1).length * 2 + 2);
    expect(many).toHaveLength(2);
    for (const b of many) expect(b.startsWith('BEGIN:VCARD') && b.endsWith('END:VCARD')).toBe(true);
    expect(cardBatches('nothing here')).toEqual([]);
  });

  it('keeps a card that is alone above the limit in a batch of its own', () => {
    expect(cardBatches([1, 2].map(card).join('\r\n'), 5)).toHaveLength(2);
  });
});

describe('the time of an agenda event', () => {
  it('shows a wall-clock time as written and a whole-day event with no hour', () => {
    const start = Date.UTC(2030, 4, 6, 9, 5) / 1000;
    expect(eventDay({ start, utc: false })).toBe('2030-05-06');
    expect(eventClock({ start, utc: false, all_day: false })).toBe('09:05');
    expect(eventClock({ start, utc: false, all_day: true })).toBe('');
  });
});
