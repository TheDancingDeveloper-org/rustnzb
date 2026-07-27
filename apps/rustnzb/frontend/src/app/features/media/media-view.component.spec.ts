import '@angular/compiler';

import { HttpClient } from '@angular/common/http';
import { MatSnackBar } from '@angular/material/snack-bar';
import { TestBed } from '@angular/core/testing';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { of, throwError } from 'rxjs';

import { ApiService } from '../../core/services/api.service';
import { MediaViewComponent } from './media-view.component';

function makeComponent(status: unknown = { webdav_enabled: false }) {
  const api = { get: vi.fn(() => of(status)) };
  const http = { request: vi.fn(() => of('')) };
  const snack = { open: vi.fn() };
  TestBed.configureTestingModule({
    providers: [
      { provide: ApiService, useValue: api },
      { provide: HttpClient, useValue: http },
      { provide: MatSnackBar, useValue: snack },
    ],
  });
  const component = TestBed.runInInjectionContext(() => new MediaViewComponent());
  return { component, api, http, snack };
}

describe('MediaViewComponent', () => {
  afterEach(() => TestBed.resetTestingModule());

  it('disables itself when WebDAV is unavailable or the status request fails', () => {
    const { component } = makeComponent({ webdav_enabled: false });
    component.ngOnInit();
    expect(component.enabled()).toBe(false);

  });

  it('disables itself when the status request fails', () => {
    const { component, api } = makeComponent();
    api.get.mockReturnValue(throwError(() => new Error('offline')));
    component.ngOnInit();
    expect(component.enabled()).toBe(false);
  });

  it('parses DAV multistatus documents and classifies streamable media', () => {
    const { component } = makeComponent();
    const items = component['parseMultiStatus'](`<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
      <d:response><d:href>/content/Release%20One/</d:href><d:propstat><d:prop><d:displayname>Release One</d:displayname><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
      <d:response><d:href>/content/Release%20One/video.mkv</d:href><d:propstat><d:prop><d:getcontentlength>2048</d:getcontentlength><d:getcontenttype>video/x-matroska</d:getcontenttype><d:resourcetype/></d:prop></d:propstat></d:response>
    </d:multistatus>`);
    expect(items).toEqual(expect.arrayContaining([
      expect.objectContaining({ href: '/content/Release One/', isDir: true }),
      expect.objectContaining({ name: 'video.mkv', size: 2048, isDir: false }),
    ]));
    expect(component.isVideo(items[1])).toBe(true);
    expect(component.isAudio({ ...items[1], name: 'track.FLAC' })).toBe(true);
    expect(component.fileUrl('/content/file.mkv')).toContain('/dav/content/file.mkv');
    expect(component.formatBytes(1536)).toBe('1.5 KB');
  });

  it('does not reload failed releases and loads files when a release is expanded', () => {
    const { component } = makeComponent();
    const failed = { href: '/content/bad', name: 'bad', files: [], expanded: false, loading: false, failMessage: 'unpack failed', queued: false };
    component.toggle(failed);
    expect(failed.expanded).toBe(false);

    const release = { ...failed, href: '/content/good', name: 'good', failMessage: null };
    const loadFiles = vi
      .spyOn(component as unknown as { loadFiles: (item: unknown) => void }, 'loadFiles')
      .mockImplementation(() => undefined);
    component.toggle(release);
    expect(release.expanded).toBe(true);
    expect(loadFiles).toHaveBeenCalledWith(release);
  });
});
