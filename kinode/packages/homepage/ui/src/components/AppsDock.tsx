import useHomepageStore, { HomepageApp } from "../store/homepageStore"
import AppDisplay from "./AppDisplay"
import usePersistentStore from "../store/persistentStore"
import { useEffect, useState } from "react"
import { isMobileCheck } from "../utils/dimensions"
import classNames from "classnames"
import { DragDropContext, Draggable, DropResult, Droppable } from 'react-beautiful-dnd'
import { getFetchUrl } from "../utils/fetch"

const AppsDock: React.FC = () => {
  const { apps } = useHomepageStore()
  const { favoriteApps, setFavoriteApps } = usePersistentStore()
  const [dockedApps, setDockedApps] = useState<HomepageApp[]>([])

  useEffect(() => {
    let final: HomepageApp[] = []
    const dockedApps = Object.entries(favoriteApps)
      .filter(([_, { favorite }]) => favorite)
      .map(([name, { order }]) => ({ ...apps.find(a => a.package_name === name), order }))
      .filter(a => a) as HomepageApp[]
    const orderedApps = dockedApps.filter(a => a.order !== undefined && a.order !== null)
    const unorderedApps = dockedApps.filter(a => a.order === undefined || a.order === null)

    for (let i = 0; i < orderedApps.length; i++) {
      final[orderedApps[i].order!] = orderedApps[i]
    }

    final = final.filter(a => a)
    unorderedApps.forEach(a => final.push(a))
    // console.log({ final })
    setDockedApps(final)
  }, [apps, favoriteApps])

  const isMobile = isMobileCheck()

  // a little function to help us with reordering the result
  const reorder = (list: HomepageApp[], startIndex: number, endIndex: number) => {
    const result = Array.from(list);
    const [removed] = result.splice(startIndex, 1);
    result.splice(endIndex, 0, removed);

    return result;
  };

  const onDragEnd = (result: DropResult) => {
    // dropped outside the list
    if (!result.destination) {
      return;
    }

    const items = reorder(
      dockedApps,
      result.source.index,
      result.destination.index
    );

    const packageNames = items.map(app => app.package_name);

    const faves = { ...favoriteApps }

    packageNames.forEach((name, i) => {
      // console.log('setting order for', name, 'to', i)
      faves[name].order = i
    })

    setFavoriteApps(faves)

    console.log({ favoriteApps })

    fetch(getFetchUrl('/order'), {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json'
      },
      credentials: 'include',
      body: JSON.stringify(packageNames)
    })
      .catch(e => console.error(e));
  }

  return <DragDropContext onDragEnd={onDragEnd}>
    <Droppable droppableId="droppable" direction="horizontal">
      {(provided, _snapshot) => (
        <div
          ref={provided.innerRef}
          {...provided.droppableProps}
          className={classNames('flex-center flex-wrap border border-orange bg-orange/25 p-2 rounded !rounded-xl', {
            'gap-8': !isMobile && dockedApps.length > 0,
            'gap-4': !isMobile && dockedApps.length === 0,
            'mb-4': !isMobile,
            'gap-4 mb-2': isMobile,
            'flex-col': dockedApps.length === 0
          })}
        >
          {/*dockedApps.length === 0
            ? <AppDisplay app={apps.find(app => app.package_name === 'app_store')!} />
            : */ dockedApps.map(app => <Draggable
            key={app.package_name}
            draggableId={app.package_name}
            index={dockedApps.indexOf(app)}
          >
            {(provided, _snapshot) => (
              <div
                ref={provided.innerRef}
                {...provided.draggableProps}
                {...provided.dragHandleProps}
              >
                <AppDisplay app={app} />
              </div>
            )}
          </Draggable>)}
          {provided.placeholder}
          {dockedApps.length === 0 && <div>Favorite an app to pin it to your dock.</div>}
        </div>
      )}
    </Droppable>
  </DragDropContext>
}

export default AppsDock