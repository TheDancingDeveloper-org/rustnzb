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
      <d:response><d:href>/dav/content/Release%20One/</d:href><d:propstat><d:prop><d:displayname>Release One</d:displayname><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
      <d:response><d:href>/dav/content/Release%20One/video.mkv</d:href><d:propstat><d:prop><d:getcontentlength>2048</d:getcontentlength><d:getcontenttype>video/x-matroska</d:getcontenttype><d:resourcetype/></d:prop></d:propstat></d:response>
    </d:multistatus>`);
    expect(items).toEqual(expect.arrayContaining([
      expect.objectContaining({ href: '/content/Release One/', isDir: true }),
      expect.objectContaining({ href: '/content/Release One/video.mkv', name: 'video.mkv', size: 2048, isDir: false }),
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

  // The server (nzbdav-dav) emits hrefs that already carry the /dav mount
  // prefix, percent-encoded, e.g. <D:href>/dav/content/README.txt</D:href>.
  const serverRootListing = `<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">
    <D:response><D:href>/dav/content</D:href><D:propstat><D:prop><D:displayname>content</D:displayname><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
    <D:response><D:href>/dav/content/My%20Show%20S01/</D:href><D:propstat><D:prop><D:displayname>My Show S01</D:displayname><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
  </D:multistatus>`;
  const serverReleaseListing = `<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">
    <D:response><D:href>/dav/content/My Show S01/</D:href><D:propstat><D:prop><D:displayname>My Show S01</D:displayname><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
    <D:response><D:href>/dav/content/My%20Show%20S01/ep%231.mkv</D:href><D:propstat><D:prop><D:getcontentlength>10</D:getcontentlength><D:resourcetype/></D:prop></D:propstat></D:response>
  </D:multistatus>`;

  it('strips the /dav mount prefix from server hrefs and still accepts root-relative hrefs', () => {
    const { component } = makeComponent();
    const parse = (href: string) =>
      component['parseMultiStatus'](
        `<D:multistatus xmlns:D="DAV:"><D:response><D:href>${href}</D:href><D:propstat><D:prop><D:resourcetype/></D:prop></D:propstat></D:response></D:multistatus>`,
      )[0];
    expect(parse('/dav/content/A%20B/c.mkv')).toEqual(
      expect.objectContaining({ href: '/content/A B/c.mkv', name: 'c.mkv' }),
    );
    expect(parse('/content/A%20B/c.mkv').href).toBe('/content/A B/c.mkv');
    expect(parse('/dav').href).toBe('/');
    // Only a whole "/dav" segment is a mount prefix.
    expect(parse('/davfoo/x').href).toBe('/davfoo/x');
  });

  it('builds playable URLs with a single /dav prefix and encoded segments', () => {
    const { component } = makeComponent();
    const origin = window.location.origin;
    expect(component.fileUrl('/content/file.mkv')).toBe(`${origin}/dav/content/file.mkv`);
    expect(component.fileUrl('/content/My Show S01/ep#1.mkv')).toBe(
      `${origin}/dav/content/My%20Show%20S01/ep%231.mkv`,
    );
  });

  it('lists releases from a real server listing without a phantom "content" entry', async () => {
    const { component, api, http } = makeComponent({ webdav_enabled: true });
    api.get.mockImplementation(((path: string) =>
      of(path === '/status' ? { webdav_enabled: true } : { queue: [], history: [] })) as never);
    http.request.mockImplementation(((_m: string, url: string) =>
      of(url.endsWith('/dav/content') ? serverRootListing : serverReleaseListing)) as never);

    component.loadContent();
    await vi.waitFor(() => expect(component.releases().length).toBe(1));
    expect(http.request).toHaveBeenCalledWith('PROPFIND', `${window.location.origin}/dav/content`, expect.anything());
    const rel = component.releases()[0];
    expect(rel).toEqual(expect.objectContaining({ href: '/content/My Show S01/', name: 'My Show S01' }));

    component.toggle(rel);
    await vi.waitFor(() => expect(rel.files.length).toBe(1));
    expect(http.request).toHaveBeenLastCalledWith(
      'PROPFIND',
      `${window.location.origin}/dav/content/My%20Show%20S01/`,
      expect.anything(),
    );
    expect(rel.files[0]).toEqual(expect.objectContaining({ href: '/content/My Show S01/ep#1.mkv', name: 'ep#1.mkv' }));
    expect(component.fileUrl(rel.files[0].href)).toBe(
      `${window.location.origin}/dav/content/My%20Show%20S01/ep%231.mkv`,
    );
  });
});
