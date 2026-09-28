import { CanActivateFn, Router } from '@angular/router';
import { inject } from '@angular/core';
import { map } from 'rxjs';
import { AuthService } from '../services/auth.service';

// Hold the navigation until the stored token is confirmed, so a stale token
// (expired, or from before a server restart) never renders protected content.
export const authGuard: CanActivateFn = () => {
  const authService = inject(AuthService);
  const router = inject(Router);

  return authService
    .ensureSession()
    .pipe(map((ok) => ok || router.createUrlTree(['/login'])));
};
