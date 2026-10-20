import { Pipe, PipeTransform, inject } from '@angular/core';
import { TranslationKey } from './en';
import { I18nService, TranslateParams } from './i18n.service';

/**
 * `{{ 'nav.downloads' | t }}` or `{{ 'status.queued' | t: { count: n } }}`.
 * The locale is a signal read inside transform, so templates re-render when it changes.
 */
@Pipe({ name: 't', standalone: true, pure: false })
export class TranslatePipe implements PipeTransform {
  private readonly i18n = inject(I18nService);

  transform(key: TranslationKey, params?: TranslateParams): string {
    return this.i18n.t(key, params);
  }
}
