import { Routes } from '@angular/router';
import { authGuard } from './core/guards/auth.guard';

export const routes: Routes = [
  { path: '', redirectTo: '/downloads', pathMatch: 'full' },
  { path: 'login', loadComponent: () => import('./features/auth/login.component').then(m => m.LoginComponent) },
  { path: 'welcome', loadComponent: () => import('./features/welcome/welcome.component').then(m => m.WelcomeComponent), canActivate: [authGuard] },
  { path: 'downloads', loadComponent: () => import('./features/queue/queue-view.component').then(m => m.QueueViewComponent), canActivate: [authGuard] },
  {
    path: 'queue',
    loadComponent: () => import('./features/queue/queue-view.component').then(m => m.QueueViewComponent),
    canActivate: [authGuard],
    data: { legacyTab: 'queue' },
  },
  {
    path: 'history',
    loadComponent: () => import('./features/queue/queue-view.component').then(m => m.QueueViewComponent),
    canActivate: [authGuard],
    data: { legacyTab: 'history' },
  },
  { path: 'rss', loadComponent: () => import('./features/rss/rss-view.component').then(m => m.RssViewComponent), canActivate: [authGuard] },
  { path: 'groups', loadComponent: () => import('./features/groups/groups-view.component').then(m => m.GroupsViewComponent), canActivate: [authGuard] },
  { path: 'settings', loadComponent: () => import('./features/settings/settings-view.component').then(m => m.SettingsViewComponent), canActivate: [authGuard] },
  { path: 'statistics', loadComponent: () => import('./features/statistics/statistics-view.component').then(m => m.StatisticsViewComponent), canActivate: [authGuard] },
  { path: 'logs', loadComponent: () => import('./features/logs/logs-view.component').then(m => m.LogsViewComponent), canActivate: [authGuard] },
  { path: 'media', loadComponent: () => import('./features/media/media-view.component').then(m => m.MediaViewComponent), canActivate: [authGuard] },
];
