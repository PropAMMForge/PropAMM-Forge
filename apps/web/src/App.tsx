import Console from '@/pages/Console'
import NewVault from '@/pages/NewVault'
import Vaults from '@/pages/Vaults'
import { Navigate, Route, Routes } from 'react-router-dom'

export function App() {
  return (
    <Routes>
      <Route path="/" element={<Console />} />
      <Route path="/vaults" element={<Vaults />} />
      <Route path="/new-vault" element={<NewVault />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  )
}
