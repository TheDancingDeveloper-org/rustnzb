import { describe, expect, it } from 'vitest';
import { AnnouncerService } from './announcer.service';

const speakable = (text: string) => text.replace(/\u200b/g, '').trim();

describe('AnnouncerService', () => {
  it('puts a routine result in the polite region', () => {
    const service = new AnnouncerService();
    service.announce('2 NZBs added to queue');

    expect(speakable(service.polite())).toBe('2 NZBs added to queue');
    expect(service.assertive()).toBe('');
  });

  it('puts a failure in the assertive region and clears the polite one', () => {
    const service = new AnnouncerService();
    service.announce('Job paused');
    service.announceError('Failed to pause 2 of 3 jobs');

    expect(speakable(service.assertive())).toBe('Failed to pause 2 of 3 jobs');
    expect(service.polite()).toBe('');
  });

  it('re-announces the same sentence as a fresh update', () => {
    const service = new AnnouncerService();
    service.announce('Copied');
    const first = service.polite();
    service.announce('Copied');

    expect(speakable(service.polite())).toBe('Copied');
    expect(service.polite()).not.toBe(first);
  });

  it('clears the other region so only one message is read', () => {
    const service = new AnnouncerService();
    service.announceError('Upload failed');
    service.announce('Upload complete');

    expect(speakable(service.polite())).toBe('Upload complete');
    expect(service.assertive()).toBe('');
  });
});
