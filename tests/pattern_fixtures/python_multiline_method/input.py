class User:
    def save(self, db, force=False) -> bool:
        db.write(self)
        return True

    def quick(self):
        pass
