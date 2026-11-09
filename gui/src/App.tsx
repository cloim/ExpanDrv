import { api } from './api';
import { createBrowserRouter, Navigate, RouterProvider, useLoaderData } from 'react-router-dom';
import { AppShell } from './shell/AppShell';
import { ActivityPage } from './pages/ActivityPage';
import { AddDriveWizard } from './pages/AddDriveWizard';
import { DrivePropertiesPage } from './pages/DrivePropertiesPage';
import { DrivesPage } from './pages/DrivesPage';
import { SettingsPage } from './pages/SettingsPage';
import { StatusProvider } from './status';
import { ToastProvider } from './ui/Toast';

// 화면마다 주소 하나: /drives, /drives/new, /drives/:id, /activity, /settings
const router = createBrowserRouter([
  {
    element: <AppShell />,
    children: [
      { index: true, loader: () => api.launchDrive(), element: <LaunchRoute /> },
      {
        path: 'drives',
        element: <DrivesPage />,
        children: [{ path: 'new', element: <AddDriveWizard /> }],
      },
      { path: 'drives/:id', element: <DrivePropertiesPage /> },
      { path: 'activity', element: <ActivityPage /> },
      { path: 'settings', element: <SettingsPage /> },
      { path: '*', element: <Navigate to="/drives" replace /> },
    ],
  },
]);

export function App() {
  return (
    <ToastProvider>
      <StatusProvider>
        <RouterProvider router={router} />
      </StatusProvider>
    </ToastProvider>
  );
}

function LaunchRoute() {
  const drive = useLoaderData() as string | null;
  return <Navigate to={drive ? `/drives/${encodeURIComponent(drive)}` : '/drives'} replace />;
}
